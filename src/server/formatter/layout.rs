// Copyright 2026 HigherOrderCO
// SPDX-License-Identifier: Apache-2.0
// Adapted from Bend 2.0.40's tools/bend-fmt-lsp/src/formatter.ts at
// 624dd9f4607cd29b0871a9b96d413bfb11e92f52. Modified for Rust: indexed
// scope/token identities, borrowed source, and allocation-free indentation.

use super::{Line, needs_space};
use std::collections::HashMap;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TokenId {
    row: usize,
    token: usize,
}

#[derive(Clone, Copy, Default)]
struct Indent {
    tabs: usize,
    spaces: usize,
}

impl Indent {
    fn write(self, output: &mut String) {
        output.extend(std::iter::repeat_n('\t', self.tabs));
        output.extend(std::iter::repeat_n(' ', self.spaces));
    }
}

#[derive(Clone, Copy)]
struct Level {
    col: usize,
    depth: usize,
    parent: Option<usize>,
}

struct MatchScope {
    parent: Option<usize>,
    first: Option<usize>,
    current: Option<usize>,
    level: Option<usize>,
    anchor: Option<TokenId>,
}

#[derive(Default)]
struct LineLayout {
    depth: usize,
    loose: bool,
    anchor: Option<TokenId>,
    bodies: Vec<InlineBody>,
}

struct Indenter {
    levels: Vec<Level>,
    scopes: Vec<MatchScope>,
    main: usize,
    level: usize,
    brackets: usize,
    was_loose: bool,
}

impl Indenter {
    fn new() -> Self {
        Self {
            levels: vec![Level {
                col: 0,
                depth: 0,
                parent: None,
            }],
            scopes: Vec::new(),
            main: 0,
            level: 0,
            brackets: 0,
            was_loose: false,
        }
    }

    fn case(&mut self, col: usize, head: bool, token: TokenId) -> Option<TokenId> {
        while let Some(scope) = self.scopes.last() {
            if scope.first.map_or_else(
                || scope.parent.is_none_or(|parent| col > parent),
                |first| col >= first,
            ) {
                break;
            }
            self.scopes.pop();
        }
        let index = self.scopes.len().checked_sub(1)?;
        if self.scopes[index].first.is_none() {
            self.scopes[index].first = Some(col);
            if head {
                if let Some(parent) = index
                    .checked_sub(1)
                    .and_then(|index| self.scopes[index].level)
                    && self.levels[self.level].depth <= self.levels[parent].depth
                {
                    self.level = self.levels.len();
                    self.levels.push(Level {
                        col,
                        depth: self.levels[parent].depth + 1,
                        parent: Some(parent),
                    });
                }
                self.scopes[index].level = Some(self.level);
            } else {
                self.scopes[index].anchor = Some(token);
            }
        } else if head && let Some(saved) = self.scopes[index].level {
            self.level = saved;
            if self.levels[saved].col != col {
                self.level = self.levels.len();
                self.levels.push(Level {
                    col,
                    ..self.levels[saved]
                });
            }
            self.scopes[index].level = Some(self.level);
        }
        self.scopes[index].current = Some(col);
        head.then_some(self.scopes[index].anchor).flatten()
    }

    fn line(&mut self, row: usize, line: &Line<'_>) -> LineLayout {
        if line.code.is_empty() && line.comment.is_empty() {
            return LineLayout::default();
        }
        let loose = self.brackets > 0 || line.tokens.is_empty();
        if !loose || !self.was_loose {
            self.level = self.main;
        }
        // Bend counts each tab as one column, rather than a display tab stop.
        let width = line.indent.len();
        while let Some(parent) = self.levels[self.level].parent {
            if width >= self.levels[self.level].col {
                break;
            }
            self.level = parent;
        }
        if width > self.levels[self.level].col {
            let parent = self.level;
            self.level = self.levels.len();
            self.levels.push(Level {
                col: width,
                depth: self.levels[parent].depth + 1,
                parent: Some(parent),
            });
        }
        let import = line
            .tokens
            .first()
            .is_some_and(|token| token.text == "import");
        let mut anchor = None;
        for (index, token) in line.tokens.iter().enumerate() {
            if token.kind == 3 && matches!(token.text, "(" | "[" | "{") {
                self.brackets += 1;
            } else if token.kind == 3 && matches!(token.text, ")" | "]" | "}") {
                self.brackets = self.brackets.saturating_sub(1);
            } else if self.brackets == 0 && !import && token.kind == 0 {
                match token.text {
                    "def" | "type" | "law" => self.scopes.clear(),
                    "match" => self.scopes.push(MatchScope {
                        parent: self.scopes.last().and_then(|scope| scope.current),
                        first: None,
                        current: None,
                        level: None,
                        anchor: self.scopes.last().and_then(|scope| scope.anchor),
                    }),
                    "case" => {
                        let current =
                            self.case(width + token.col, index == 0, TokenId { row, token: index });
                        if index == 0 {
                            anchor = current;
                        }
                    }
                    _ => {}
                }
            }
        }
        if !loose {
            self.main = self.level;
        }
        self.was_loose = loose;
        LineLayout {
            depth: self.levels[self.level].depth,
            loose,
            anchor,
            bodies: Vec::new(),
        }
    }
}

struct BodyScope {
    open: usize,
    angle: usize,
    end: usize,
}

struct InlineBody {
    at: usize,
    scope: Option<usize>,
}

struct Header {
    open: usize,
    angle: isize,
    typed: bool,
}

struct BodyScanner {
    open: usize,
    angles: Vec<usize>,
    parens: Vec<usize>,
    active: Vec<usize>,
    scopes: Vec<BodyScope>,
    header: Option<Header>,
    end: usize,
}

impl BodyScanner {
    fn new(end: usize) -> Self {
        Self {
            open: 0,
            angles: vec![0],
            parens: Vec::new(),
            active: Vec::new(),
            scopes: Vec::new(),
            header: None,
            end,
        }
    }

    fn scope(&mut self) {
        self.active.push(self.scopes.len());
        self.scopes.push(BodyScope {
            open: self.open,
            angle: self.angles[self.open],
            end: self.end,
        });
    }

    fn close_scopes(&mut self, row: usize, index: usize, comma: bool) {
        while let Some(scope) = self.active.last().copied() {
            let scope = &mut self.scopes[scope];
            let inside = scope.open < self.open
                || (scope.open == self.open && scope.angle <= self.angles[self.open]);
            let separator =
                comma && scope.open == self.open && scope.angle == self.angles[self.open];
            if inside && !separator {
                break;
            }
            scope.end = row.saturating_sub(usize::from(index == 0));
            self.active.pop();
        }
    }

    fn line(&mut self, row: usize, line: &Line<'_>) -> Vec<InlineBody> {
        let mut bodies = Vec::new();
        for (index, token) in line.tokens.iter().enumerate() {
            let next = index + 1 < line.tokens.len();
            let mut body = false;
            if token.kind == 0 && matches!(token.text, "do" | "match" | "case" | "def") {
                self.header = Some(Header {
                    open: self.open,
                    angle: 0,
                    typed: matches!(token.text, "do" | "def"),
                });
            } else if matches!(token.text, "=>" | ";") {
                if self.open > 0 {
                    self.scope();
                }
                body = true;
            } else if matches!(token.text, "(" | "[" | "{") {
                self.open += 1;
                self.angles.push(0);
                if token.text == "(" {
                    self.parens.push(self.open);
                    if next {
                        self.scope();
                        body = true;
                    }
                }
            } else if matches!(token.text, ")" | "]" | "}") {
                if self.parens.last() == Some(&self.open) {
                    self.parens.pop();
                }
                if self.open > 0 {
                    self.angles.pop();
                    self.open -= 1;
                }
            } else if matches!(token.text, "<" | ">" | "<<" | ">>") {
                match token.text {
                    "<" => self.angles[self.open] += 1,
                    ">" => self.angles[self.open] = self.angles[self.open].saturating_sub(1),
                    ">>" => self.angles[self.open] = self.angles[self.open].saturating_sub(2),
                    _ => {}
                }
                if let Some(header) = &mut self.header
                    && header.typed
                    && header.open == self.open
                {
                    header.angle += match token.text {
                        "<" => 1,
                        "<<" => 2,
                        ">" => -1,
                        _ => -2,
                    };
                }
            } else if token.text == ":"
                && self
                    .header
                    .as_ref()
                    .is_some_and(|header| header.open == self.open && header.angle == 0)
            {
                self.header = None;
                body = true;
            }
            if body {
                if next {
                    bodies.push(InlineBody {
                        at: index + 1,
                        scope: self.active.last().copied(),
                    });
                }
                continue;
            }
            self.close_scopes(row, index, token.text == ",");
            if token.text == ","
                && self.parens.last() == Some(&self.open)
                && self.angles[self.open] == 0
                && next
            {
                self.scope();
                bodies.push(InlineBody {
                    at: index + 1,
                    scope: self.active.last().copied(),
                });
            }
        }
        bodies
    }
}

struct Pin {
    from: usize,
    offset: usize,
    prefix: Indent,
    end: usize,
}

pub(super) struct Layout {
    lines: Vec<LineLayout>,
    scopes: Vec<BodyScope>,
    pins: Vec<Pin>,
    active: Vec<usize>,
    by_body: HashMap<TokenId, usize>,
}

impl Layout {
    pub(super) fn new(lines: &[Line<'_>]) -> Self {
        let mut indenter = Indenter::new();
        let mut scanner = BodyScanner::new(lines.len());
        let lines = lines
            .iter()
            .enumerate()
            .map(|(row, line)| {
                let mut layout = indenter.line(row, line);
                layout.bodies = scanner.line(row, line);
                layout
            })
            .collect();
        Self {
            lines,
            scopes: scanner.scopes,
            pins: Vec::new(),
            active: Vec::new(),
            by_body: HashMap::new(),
        }
    }

    fn prefix(&mut self, row: usize, width: usize, size: usize, spaces: bool) -> Indent {
        let shape = &self.lines[row];
        let pins = &self.pins;
        self.active.retain(|pin| pins[*pin].end >= row);
        if !shape.loose {
            while self
                .active
                .last()
                .is_some_and(|pin| pins[*pin].from > width)
            {
                self.active.pop();
            }
        }
        let selected = if let Some(anchor) = shape.anchor {
            self.by_body.get(&anchor).copied()
        } else {
            self.active
                .iter()
                .rev()
                .copied()
                .find(|pin| pins[*pin].from <= width)
        };
        if let Some(selected) = selected {
            if shape.anchor.is_some() && self.active.last() != Some(&selected) {
                self.active.push(selected);
            }
            let pin = &pins[selected];
            return Indent {
                tabs: pin.prefix.tabs,
                spaces: pin.prefix.spaces + width.saturating_sub(pin.from) + pin.offset,
            };
        }
        if spaces {
            Indent {
                tabs: 0,
                spaces: shape.depth * size,
            }
        } else {
            Indent {
                tabs: shape.depth,
                spaces: 0,
            }
        }
    }

    pub(super) fn write_line(
        &mut self,
        row: usize,
        line: &Line<'_>,
        size: usize,
        spaces: bool,
        output: &mut String,
    ) {
        if line.code.is_empty() && line.comment.is_empty() {
            return;
        }
        let prefix = self.prefix(row, line.indent.len(), size, spaces);
        prefix.write(output);
        let mut column = 0;
        let mut bodies = self.lines[row].bodies.iter().peekable();
        for (index, token) in line.tokens.iter().enumerate() {
            if index > 0 && needs_space(&line.tokens, index) {
                output.push(' ');
                column += 1;
            }
            while bodies.peek().is_some_and(|body| body.at == index) {
                let Some(body) = bodies.next() else { break };
                let id = self.pins.len();
                self.pins.push(Pin {
                    from: line.indent.len() + token.col,
                    offset: column,
                    prefix,
                    end: body
                        .scope
                        .map_or(self.lines.len(), |scope| self.scopes[scope].end),
                });
                self.active.push(id);
                self.by_body.insert(TokenId { row, token: index }, id);
            }
            output.push_str(token.text);
            column += token.width;
        }
        if !line.comment.is_empty() {
            if !line.tokens.is_empty() {
                output.push_str("  ");
            }
            output.push_str(line.comment);
        }
    }
}

pub(super) type Fingerprint<'a> = Vec<(usize, Vec<&'a str>)>;

pub(super) fn fingerprint<'a>(lines: &[Line<'a>]) -> Fingerprint<'a> {
    let mut indenter = Indenter::new();
    lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            let layout = indenter.line(row, line);
            let depth = if line.tokens.is_empty() {
                0
            } else {
                layout.depth
            };
            (depth, line.tokens.iter().map(|token| token.text).collect())
        })
        .collect()
}

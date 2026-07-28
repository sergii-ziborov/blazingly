//! Bounded, backtracking-free pattern matching used by `#[pattern("...")]`.

use core::fmt;

/// Maximum accepted pattern length in characters.
pub const MAX_PATTERN_CHARS: usize = 512;

/// Maximum number of instructions a compiled pattern may occupy.
pub const MAX_PATTERN_INSTRUCTIONS: usize = 1024;

/// Maximum accepted group nesting depth.
pub const MAX_PATTERN_DEPTH: usize = 16;

/// Maximum accepted member count inside one character class.
pub const MAX_CLASS_MEMBERS: usize = 128;

/// A rejected pattern, reported instead of an unbounded match attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternError {
    /// The pattern text was empty.
    Empty,
    /// The pattern exceeded [`MAX_PATTERN_CHARS`].
    TooLong,
    /// Group nesting exceeded [`MAX_PATTERN_DEPTH`].
    TooDeep,
    /// Compilation exceeded [`MAX_PATTERN_INSTRUCTIONS`].
    TooLarge,
    /// A group was opened but never closed, or closed but never opened.
    UnbalancedGroup,
    /// A character class was opened but never closed.
    UnbalancedClass,
    /// A character class contained no members.
    EmptyClass,
    /// A character class exceeded [`MAX_CLASS_MEMBERS`].
    ClassTooLarge,
    /// A class range ran from a higher code point to a lower one.
    ReversedRange,
    /// A quantifier had no preceding atom, or followed another quantifier.
    DanglingQuantifier,
    /// The pattern ended with an unfinished escape.
    TrailingEscape,
    /// The escape sequence is outside the supported subset.
    UnsupportedEscape(char),
    /// The syntax element is outside the supported subset.
    UnsupportedSyntax(char),
}

impl fmt::Display for PatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the pattern is empty"),
            Self::TooLong => write!(
                formatter,
                "the pattern exceeds {MAX_PATTERN_CHARS} characters"
            ),
            Self::TooDeep => write!(formatter, "groups nest deeper than {MAX_PATTERN_DEPTH}"),
            Self::TooLarge => write!(
                formatter,
                "the pattern compiles to more than {MAX_PATTERN_INSTRUCTIONS} instructions"
            ),
            Self::UnbalancedGroup => formatter.write_str("the pattern has an unbalanced group"),
            Self::UnbalancedClass => {
                formatter.write_str("the pattern has an unterminated character class")
            }
            Self::EmptyClass => formatter.write_str("a character class has no members"),
            Self::ClassTooLarge => write!(
                formatter,
                "a character class has more than {MAX_CLASS_MEMBERS} members"
            ),
            Self::ReversedRange => formatter.write_str("a character range runs backwards"),
            Self::DanglingQuantifier => {
                formatter.write_str("a quantifier has no preceding expression")
            }
            Self::TrailingEscape => formatter.write_str("the pattern ends with a lone backslash"),
            Self::UnsupportedEscape(value) => {
                write!(formatter, "the escape `\\{value}` is not supported")
            }
            Self::UnsupportedSyntax(value) => {
                write!(formatter, "the syntax element `{value}` is not supported")
            }
        }
    }
}

impl std::error::Error for PatternError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClassMember {
    Exact(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

impl ClassMember {
    fn contains(self, value: char) -> bool {
        match self {
            Self::Exact(expected) => value == expected,
            Self::Range(start, end) => value >= start && value <= end,
            Self::Digit => value.is_ascii_digit(),
            Self::NotDigit => !value.is_ascii_digit(),
            Self::Word => value.is_ascii_alphanumeric() || value == '_',
            Self::NotWord => !(value.is_ascii_alphanumeric() || value == '_'),
            Self::Space => value.is_ascii_whitespace(),
            Self::NotSpace => !value.is_ascii_whitespace(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CharacterClass {
    negated: bool,
    members: Vec<ClassMember>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Matcher {
    Any,
    Class(CharacterClass),
}

impl Matcher {
    fn contains(&self, value: char) -> bool {
        match self {
            Self::Any => value != '\n',
            Self::Class(class) => {
                class.members.iter().any(|member| member.contains(value)) != class.negated
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Node {
    Sequence(Vec<Node>),
    Choice(Vec<Node>),
    Single(Matcher),
    Optional(Box<Node>),
    ZeroOrMore(Box<Node>),
    OneOrMore(Box<Node>),
}

#[derive(Clone, Copy, Debug)]
enum Instruction {
    Consume(usize),
    Split(usize, usize),
    Jump(usize),
    Accept,
}

struct Parser<'source> {
    source: &'source [char],
    position: usize,
    depth: usize,
}

impl<'source> Parser<'source> {
    const fn new(source: &'source [char]) -> Self {
        Self {
            source,
            position: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.source.get(self.position).copied()
    }

    fn peek_ahead(&self, offset: usize) -> Option<char> {
        self.source.get(self.position + offset).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.position += 1;
        Some(value)
    }

    fn parse_choice(&mut self) -> Result<Node, PatternError> {
        let mut branches = vec![self.parse_sequence()?];
        while self.peek() == Some('|') {
            self.position += 1;
            branches.push(self.parse_sequence()?);
        }
        Ok(Node::Choice(branches))
    }

    fn parse_sequence(&mut self) -> Result<Node, PatternError> {
        let mut nodes = Vec::new();
        while let Some(value) = self.peek() {
            if value == '|' || value == ')' {
                break;
            }
            let atom = self.parse_atom()?;
            nodes.push(self.parse_quantifier(atom)?);
        }
        Ok(Node::Sequence(nodes))
    }

    fn parse_quantifier(&mut self, node: Node) -> Result<Node, PatternError> {
        let Some(value) = self.peek() else {
            return Ok(node);
        };
        let quantified = match value {
            '*' => Node::ZeroOrMore(Box::new(node)),
            '+' => Node::OneOrMore(Box::new(node)),
            '?' => Node::Optional(Box::new(node)),
            _ => return Ok(node),
        };
        self.position += 1;
        if matches!(self.peek(), Some('*' | '+' | '?')) {
            return Err(PatternError::DanglingQuantifier);
        }
        Ok(quantified)
    }

    fn parse_atom(&mut self) -> Result<Node, PatternError> {
        let Some(value) = self.advance() else {
            return Err(PatternError::UnbalancedGroup);
        };
        match value {
            '(' => self.parse_group(),
            ')' => Err(PatternError::UnbalancedGroup),
            '[' => self.parse_class(),
            ']' => Err(PatternError::UnbalancedClass),
            '.' => Ok(Node::Single(Matcher::Any)),
            '\\' => {
                let Some(escaped) = self.advance() else {
                    return Err(PatternError::TrailingEscape);
                };
                Ok(Node::Single(Matcher::Class(CharacterClass {
                    negated: false,
                    members: vec![escape_member(escaped)?],
                })))
            }
            '*' | '+' | '?' => Err(PatternError::DanglingQuantifier),
            '{' | '}' | '^' | '$' => Err(PatternError::UnsupportedSyntax(value)),
            _ => Ok(Node::Single(Matcher::Class(CharacterClass {
                negated: false,
                members: vec![ClassMember::Exact(value)],
            }))),
        }
    }

    fn parse_group(&mut self) -> Result<Node, PatternError> {
        if self.peek() == Some('?') {
            return Err(PatternError::UnsupportedSyntax('?'));
        }
        self.depth += 1;
        if self.depth > MAX_PATTERN_DEPTH {
            return Err(PatternError::TooDeep);
        }
        let inner = self.parse_choice()?;
        if self.advance() != Some(')') {
            return Err(PatternError::UnbalancedGroup);
        }
        self.depth -= 1;
        Ok(inner)
    }

    fn parse_class(&mut self) -> Result<Node, PatternError> {
        let negated = self.peek() == Some('^');
        if negated {
            self.position += 1;
        }
        let mut members = Vec::new();
        loop {
            let Some(value) = self.advance() else {
                return Err(PatternError::UnbalancedClass);
            };
            if value == ']' {
                break;
            }
            if members.len() >= MAX_CLASS_MEMBERS {
                return Err(PatternError::ClassTooLarge);
            }
            members.push(self.parse_class_member(value)?);
        }
        if members.is_empty() {
            return Err(PatternError::EmptyClass);
        }
        Ok(Node::Single(Matcher::Class(CharacterClass {
            negated,
            members,
        })))
    }

    fn parse_class_member(&mut self, value: char) -> Result<ClassMember, PatternError> {
        let member = if value == '\\' {
            let Some(escaped) = self.advance() else {
                return Err(PatternError::TrailingEscape);
            };
            escape_member(escaped)?
        } else {
            ClassMember::Exact(value)
        };
        let ClassMember::Exact(start) = member else {
            return Ok(member);
        };
        if self.peek() != Some('-') || matches!(self.peek_ahead(1), None | Some(']')) {
            return Ok(member);
        }
        self.position += 1;
        let Some(end) = self.advance() else {
            return Err(PatternError::UnbalancedClass);
        };
        let end = if end == '\\' {
            let Some(escaped) = self.advance() else {
                return Err(PatternError::TrailingEscape);
            };
            match escape_member(escaped)? {
                ClassMember::Exact(value) => value,
                _ => return Err(PatternError::UnsupportedEscape(escaped)),
            }
        } else {
            end
        };
        if end < start {
            return Err(PatternError::ReversedRange);
        }
        Ok(ClassMember::Range(start, end))
    }
}

fn escape_member(value: char) -> Result<ClassMember, PatternError> {
    Ok(match value {
        'd' => ClassMember::Digit,
        'D' => ClassMember::NotDigit,
        'w' => ClassMember::Word,
        'W' => ClassMember::NotWord,
        's' => ClassMember::Space,
        'S' => ClassMember::NotSpace,
        't' => ClassMember::Exact('\t'),
        'n' => ClassMember::Exact('\n'),
        'r' => ClassMember::Exact('\r'),
        '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$'
        | '-' | '/' => ClassMember::Exact(value),
        _ => return Err(PatternError::UnsupportedEscape(value)),
    })
}

struct Program {
    instructions: Vec<Instruction>,
    matchers: Vec<Matcher>,
}

impl Program {
    fn push(&mut self, instruction: Instruction) -> Result<usize, PatternError> {
        if self.instructions.len() >= MAX_PATTERN_INSTRUCTIONS {
            return Err(PatternError::TooLarge);
        }
        self.instructions.push(instruction);
        Ok(self.instructions.len() - 1)
    }

    fn patch(&mut self, slot: usize, instruction: Instruction) {
        if let Some(target) = self.instructions.get_mut(slot) {
            *target = instruction;
        }
    }

    fn emit(&mut self, node: &Node) -> Result<(), PatternError> {
        match node {
            Node::Sequence(nodes) => {
                for node in nodes {
                    self.emit(node)?;
                }
                Ok(())
            }
            Node::Choice(branches) => self.emit_choice(branches),
            Node::Single(matcher) => {
                let index = self.matchers.len();
                self.matchers.push(matcher.clone());
                self.push(Instruction::Consume(index)).map(|_| ())
            }
            Node::Optional(inner) => {
                let split = self.push(Instruction::Split(0, 0))?;
                let start = self.instructions.len();
                self.emit(inner)?;
                let end = self.instructions.len();
                self.patch(split, Instruction::Split(start, end));
                Ok(())
            }
            Node::ZeroOrMore(inner) => {
                let split = self.push(Instruction::Split(0, 0))?;
                let start = self.instructions.len();
                self.emit(inner)?;
                self.push(Instruction::Jump(split))?;
                let end = self.instructions.len();
                self.patch(split, Instruction::Split(start, end));
                Ok(())
            }
            Node::OneOrMore(inner) => {
                let start = self.instructions.len();
                self.emit(inner)?;
                let split = self.push(Instruction::Split(0, 0))?;
                self.patch(split, Instruction::Split(start, split + 1));
                Ok(())
            }
        }
    }

    fn emit_choice(&mut self, branches: &[Node]) -> Result<(), PatternError> {
        let Some((last, leading)) = branches.split_last() else {
            return Ok(());
        };
        let mut jumps = Vec::new();
        for branch in leading {
            let split = self.push(Instruction::Split(0, 0))?;
            let start = self.instructions.len();
            self.emit(branch)?;
            jumps.push(self.push(Instruction::Jump(0))?);
            let next = self.instructions.len();
            self.patch(split, Instruction::Split(start, next));
        }
        self.emit(last)?;
        let end = self.instructions.len();
        for jump in jumps {
            self.patch(jump, Instruction::Jump(end));
        }
        Ok(())
    }
}

/// Working buffers reused across matches on one thread.
///
/// The simulation needs a mark table and two alternating state sets. Their
/// sizes are bounded by the compiled program, which is itself bounded, so
/// holding them per thread costs a few kilobytes and removes every allocation
/// from the matching path.
#[derive(Default)]
struct Scratch {
    marks: Vec<usize>,
    active: Vec<usize>,
    next: Vec<usize>,
}

thread_local! {
    static SCRATCH: core::cell::RefCell<Scratch> =
        const { core::cell::RefCell::new(Scratch {
            marks: Vec::new(),
            active: Vec::new(),
            next: Vec::new(),
        }) };
}

/// A compiled pattern that scans input in a single left-to-right pass.
///
/// Matching simulates every alternative in parallel, so run time is linear in
/// the input length and no input can trigger exponential backtracking.
#[derive(Debug)]
pub struct Pattern {
    instructions: Vec<Instruction>,
    matchers: Vec<Matcher>,
    anchored_start: bool,
    anchored_end: bool,
}

impl Pattern {
    /// Compiles the supported subset of regular-expression syntax.
    ///
    /// Anchors `^` and `$` are honoured at the pattern edges. Groups,
    /// alternation, character classes, `.`, `*`, `+`, `?`, and the ASCII
    /// escapes `\d`, `\D`, `\w`, `\W`, `\s`, `\S`, `\t`, `\n`, `\r` are
    /// supported. Counted repetition, backreferences, and lookaround are not.
    ///
    /// # Errors
    ///
    /// Returns [`PatternError`] when the pattern uses unsupported syntax or
    /// exceeds one of the module's size limits.
    pub fn compile(source: &str) -> Result<Self, PatternError> {
        if source.is_empty() {
            return Err(PatternError::Empty);
        }
        let characters = source.chars().collect::<Vec<_>>();
        if characters.len() > MAX_PATTERN_CHARS {
            return Err(PatternError::TooLong);
        }
        let anchored_start = characters.first() == Some(&'^');
        let body_start = usize::from(anchored_start);
        let anchored_end = ends_with_anchor(&characters[body_start..]);
        let body_end = characters.len() - usize::from(anchored_end);
        let body = characters
            .get(body_start..body_end)
            .ok_or(PatternError::UnbalancedGroup)?;

        let mut parser = Parser::new(body);
        let node = parser.parse_choice()?;
        if parser.position != body.len() {
            return Err(PatternError::UnbalancedGroup);
        }

        let mut program = Program {
            instructions: Vec::new(),
            matchers: Vec::new(),
        };
        program.emit(&node)?;
        program.push(Instruction::Accept)?;

        Ok(Self {
            instructions: program.instructions,
            matchers: program.matchers,
            anchored_start,
            anchored_end,
        })
    }

    /// Reports whether the value satisfies the pattern.
    ///
    /// The simulation runs on per-thread scratch buffers rather than fresh
    /// allocations. It used to allocate a mark table and an active set per
    /// call, plus one successor set *per input character*: matching a
    /// thirteen-character slug cost about fifteen allocations, and a bulk
    /// request validating fifty items against two pattern rules cost roughly
    /// fifteen hundred. Under four worker threads that turned into allocator
    /// contention rather than work, and it showed as a server that used 117% of
    /// one core where its peers used 250-280% on the same request, and that got
    /// *slower* going from one connection to sixty-four.
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        SCRATCH.with_borrow_mut(|scratch| self.matches_with(value, scratch))
    }

    fn matches_with(&self, value: &str, scratch: &mut Scratch) -> bool {
        let Scratch {
            marks,
            active,
            next,
        } = scratch;
        marks.clear();
        marks.resize(self.instructions.len(), usize::MAX);
        active.clear();
        next.clear();

        let mut generation = 0_usize;
        self.add_thread(active, marks, generation, 0);

        for character in value.chars() {
            if !self.anchored_end && self.accepts(active) {
                return true;
            }
            generation += 1;
            next.clear();
            for &index in active.iter() {
                if let Some(Instruction::Consume(matcher)) = self.instructions.get(index)
                    && self
                        .matchers
                        .get(*matcher)
                        .is_some_and(|matcher| matcher.contains(character))
                {
                    self.add_thread(next, marks, generation, index + 1);
                }
            }
            if !self.anchored_start {
                self.add_thread(next, marks, generation, 0);
            }
            std::mem::swap(active, next);
            if active.is_empty() && self.anchored_start {
                return false;
            }
        }

        self.accepts(active)
    }

    /// Reports whether the value satisfies the pattern, without touching the
    /// shared scratch buffers.
    ///
    /// [`Pattern::matches`] borrows a thread-local scratch set, so a caller
    /// already inside that borrow cannot call it again. Nothing in this crate
    /// does, but a future matcher that recursed would deadlock rather than
    /// misbehave quietly, and this entry point is the way out.
    #[must_use]
    pub fn matches_isolated(&self, value: &str) -> bool {
        let mut scratch = Scratch::default();
        self.matches_with(value, &mut scratch)
    }

    fn accepts(&self, active: &[usize]) -> bool {
        active
            .iter()
            .any(|index| matches!(self.instructions.get(*index), Some(Instruction::Accept)))
    }

    fn add_thread(
        &self,
        active: &mut Vec<usize>,
        marks: &mut [usize],
        generation: usize,
        start: usize,
    ) {
        let mut pending = vec![start];
        while let Some(index) = pending.pop() {
            if marks.get(index).copied() == Some(generation) {
                continue;
            }
            if let Some(mark) = marks.get_mut(index) {
                *mark = generation;
            }
            match self.instructions.get(index) {
                Some(Instruction::Jump(target)) => pending.push(*target),
                Some(Instruction::Split(first, second)) => {
                    pending.push(*second);
                    pending.push(*first);
                }
                Some(Instruction::Consume(_) | Instruction::Accept) => active.push(index),
                None => {}
            }
        }
    }
}

fn ends_with_anchor(body: &[char]) -> bool {
    if body.last() != Some(&'$') {
        return false;
    }
    let backslashes = body
        .iter()
        .rev()
        .skip(1)
        .take_while(|value| **value == '\\')
        .count();
    backslashes % 2 == 0
}

/// Compiles and applies a pattern, reporting `false` for unsupported patterns.
///
/// Compilation is memoized per thread, so repeatedly applying one pattern pays
/// the parse cost once.
#[must_use]
pub fn matches_pattern(value: &str, pattern: &str) -> bool {
    crate::cache::compiled_pattern(pattern).is_ok_and(|compiled| compiled.matches(value))
}

#[cfg(test)]
mod tests {
    use super::{MAX_PATTERN_CHARS, Pattern, PatternError, matches_pattern};

    #[test]
    fn anchored_classes_and_quantifiers_match_expected_values() {
        let pattern = Pattern::compile("^[a-z][a-z0-9_]*$").expect("pattern compiles");
        assert!(pattern.matches("user_42"));
        assert!(pattern.matches("a"));
        assert!(!pattern.matches("User"));
        assert!(!pattern.matches("4user"));
        assert!(!pattern.matches(""));
    }

    #[test]
    fn alternation_groups_and_escapes_are_supported() {
        let pattern = Pattern::compile(r"^(cat|dog|bird)-\d+$").expect("pattern compiles");
        assert!(pattern.matches("cat-1"));
        assert!(pattern.matches("bird-9001"));
        assert!(!pattern.matches("fish-1"));
        assert!(!pattern.matches("cat-"));
    }

    #[test]
    fn unanchored_patterns_search_anywhere_and_anchors_restrict() {
        assert!(matches_pattern("abc-123", r"\d+"));
        assert!(!matches_pattern("abc-123", r"^\d+"));
        assert!(matches_pattern("123-abc", r"^\d+"));
        assert!(matches_pattern("abc-123", r"\d+$"));
        assert!(!matches_pattern("123-abc", r"\d+$"));
    }

    #[test]
    fn nested_quantifiers_do_not_blow_up_on_hostile_input() {
        let pattern = Pattern::compile("^(a+)+$").expect("pattern compiles");
        let hostile = "a".repeat(4096) + "b";
        assert!(!pattern.matches(&hostile));
        assert!(pattern.matches(&"a".repeat(4096)));
    }

    #[test]
    fn negated_classes_dot_and_optional_behave_like_the_documented_subset() {
        assert!(matches_pattern("ab", "^a[^0-9]$"));
        assert!(!matches_pattern("a9", "^a[^0-9]$"));
        assert!(matches_pattern("color", "^colou?r$"));
        assert!(matches_pattern("colour", "^colou?r$"));
        assert!(matches_pattern("a\tb", "^a.b$"));
        assert!(!matches_pattern("a\nb", "^a.b$"));
    }

    fn rejection(source: &str) -> Option<PatternError> {
        Pattern::compile(source).err()
    }

    #[test]
    fn unsupported_syntax_is_rejected_at_compile_time() {
        assert_eq!(rejection(""), Some(PatternError::Empty));
        assert_eq!(
            rejection("^a{2,3}$"),
            Some(PatternError::UnsupportedSyntax('{'))
        );
        assert_eq!(
            rejection("^(?:a)$"),
            Some(PatternError::UnsupportedSyntax('?'))
        );
        assert_eq!(rejection("^(a$"), Some(PatternError::UnbalancedGroup));
        assert_eq!(rejection("^[a-z$"), Some(PatternError::UnbalancedClass));
        assert_eq!(rejection("^*a$"), Some(PatternError::DanglingQuantifier));
        assert_eq!(
            rejection(r"^\q$"),
            Some(PatternError::UnsupportedEscape('q'))
        );
        assert_eq!(rejection(r"^a\"), Some(PatternError::TrailingEscape));
        assert_eq!(rejection("^[z-a]$"), Some(PatternError::ReversedRange));
        assert_eq!(
            rejection(&format!("^{}$", "a".repeat(MAX_PATTERN_CHARS))),
            Some(PatternError::TooLong)
        );
    }

    #[test]
    fn escaped_trailing_dollar_is_a_literal_not_an_anchor() {
        assert!(matches_pattern("cost$", r"^cost\$"));
        assert!(matches_pattern("cost$ and more", r"^cost\$"));
    }
}

//! Bounded, backtracking-free pattern matching used by `#[pattern("...")]`.
//!
//! A pattern is parsed once, compiled to a Thompson program once, and then
//! matched against one value per request for the life of the process. That
//! asymmetry is what the module is shaped around: compilation may spend work to
//! find a specialized form, and matching runs whichever form was found.
//!
//! [`Pattern::compile`] picks one of three engines.
//!
//! * A finite literal set, when the pattern is anchored at both ends and its
//!   language is a bounded list of literal strings. `^(uk|ru|en)$` is a length
//!   test and a couple of comparisons, not a state machine, and enumerated
//!   fields of that shape are common enough in real models to deserve the case.
//! * A deterministic byte automaton, when every character class is ASCII-only
//!   and the subset construction stays inside its bounds. See [`dfa`].
//! * The Thompson simulation, for everything else.
//!
//! Every engine answers identically; the simulation is the definition, and
//! [`Pattern::matches_simulated`] exposes it so the others can be checked
//! against it. Matching is linear in the input length under all three: the
//! specialized engines are built during compilation, never while matching, so
//! no input can provoke backtracking.

use core::fmt;

mod dfa;

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

    /// Reports whether every character this member accepts is ASCII.
    ///
    /// The negated forms accept every character outside their own set, which
    /// includes every non-ASCII character, so they answer `false`.
    const fn is_ascii_only(self) -> bool {
        match self {
            Self::Exact(value) => value.is_ascii(),
            // A range is ordered, so its upper bound decides.
            Self::Range(_, end) => end.is_ascii(),
            Self::Digit | Self::Word | Self::Space => true,
            Self::NotDigit | Self::NotWord | Self::NotSpace => false,
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

    /// Reports whether every character this matcher accepts is ASCII.
    ///
    /// `.` accepts any character but a newline, and a negated class accepts
    /// everything it does not list, so both answer `false`.
    fn is_ascii_only(&self) -> bool {
        match self {
            Self::Any => false,
            Self::Class(class) => {
                !class.negated && class.members.iter().all(|member| member.is_ascii_only())
            }
        }
    }

    /// Number of class members this matcher tests, used to bound classifier
    /// work.
    fn member_count(&self) -> usize {
        match self {
            Self::Any => 1,
            Self::Class(class) => class.members.len(),
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

/// Largest number of literal alternatives the literal engine is built for.
const MAX_LITERAL_ALTERNATIVES: usize = 32;

/// Largest literal length the literal engine is built for.
const MAX_LITERAL_LENGTH: usize = 64;

/// Enumerates the literal strings a node matches, when there are finitely many.
///
/// Returns `None` as soon as the node admits a quantifier, a wildcard, a
/// character class with more than one exact member, or more alternatives than
/// the bounds above allow. A `None` here is never a wrong answer, only a
/// missed specialization: the caller falls through to another engine.
fn literal_language(node: &Node) -> Option<Vec<String>> {
    match node {
        Node::Single(Matcher::Class(class)) if !class.negated => match class.members.as_slice() {
            [ClassMember::Exact(value)] => Some(vec![value.to_string()]),
            _ => None,
        },
        Node::Sequence(nodes) => {
            let mut alternatives = vec![String::new()];
            for node in nodes {
                let tail = literal_language(node)?;
                if tail.is_empty() || alternatives.len() * tail.len() > MAX_LITERAL_ALTERNATIVES {
                    return None;
                }
                let mut combined = Vec::with_capacity(alternatives.len() * tail.len());
                for prefix in &alternatives {
                    for suffix in &tail {
                        if prefix.len() + suffix.len() > MAX_LITERAL_LENGTH {
                            return None;
                        }
                        combined.push(format!("{prefix}{suffix}"));
                    }
                }
                alternatives = combined;
            }
            Some(alternatives)
        }
        Node::Choice(branches) => {
            let mut alternatives = Vec::new();
            for branch in branches {
                alternatives.extend(literal_language(branch)?);
                if alternatives.len() > MAX_LITERAL_ALTERNATIVES {
                    return None;
                }
            }
            Some(alternatives)
        }
        Node::Single(Matcher::Any | Matcher::Class(_))
        | Node::Optional(_)
        | Node::ZeroOrMore(_)
        | Node::OneOrMore(_) => None,
    }
}

/// A doubly anchored pattern whose language is a finite set of literals.
///
/// Matching is an equality test against each literal. The length gate rejects
/// most non-matching values before any byte is compared, which matters because
/// the values a rule rejects are as much part of the workload as the ones it
/// accepts.
#[derive(Debug)]
struct LiteralSet {
    literals: Box<[Box<str>]>,
    shortest: usize,
    longest: usize,
}

impl LiteralSet {
    fn new(mut literals: Vec<String>) -> Self {
        literals.sort_unstable();
        literals.dedup();
        let shortest = literals.iter().map(String::len).min().unwrap_or(usize::MAX);
        let longest = literals.iter().map(String::len).max().unwrap_or(0);
        Self {
            literals: literals.into_iter().map(String::into_boxed_str).collect(),
            shortest,
            longest,
        }
    }

    fn matches(&self, value: &str) -> bool {
        if value.len() < self.shortest || value.len() > self.longest {
            return false;
        }
        self.literals.iter().any(|literal| &**literal == value)
    }
}

/// The engine [`Pattern::matches`] dispatches to.
#[derive(Debug)]
enum Engine {
    /// A finite set of literals, compared directly.
    Literals(LiteralSet),
    /// A deterministic automaton over byte classes.
    Deterministic(Box<dfa::Dfa>),
    /// The Thompson simulation, which every pattern can always fall back on.
    Simulation,
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
/// The simulation needs a mark table, two alternating state sets, and a work
/// list for the closure. Their sizes are bounded by the compiled program, which
/// is itself bounded, so holding them per thread costs a few kilobytes and
/// removes every allocation from the matching path.
///
/// The marks are stamped with an epoch rather than cleared. Clearing them cost
/// one write per instruction per call; the epoch only ever increases, so a mark
/// left by an earlier call — or by an earlier *pattern*, since one thread's
/// buffers serve every pattern it evaluates — can never be mistaken for a mark
/// from the current one.
#[derive(Default)]
struct Scratch {
    marks: Vec<u64>,
    active: Vec<usize>,
    next: Vec<usize>,
    pending: Vec<usize>,
    generation: u64,
}

thread_local! {
    static SCRATCH: core::cell::RefCell<Scratch> =
        const { core::cell::RefCell::new(Scratch {
            marks: Vec::new(),
            active: Vec::new(),
            next: Vec::new(),
            pending: Vec::new(),
            generation: 0,
        }) };
}

/// Adds the epsilon closure of `start` to the thread set.
///
/// Instructions already stamped with `generation` are skipped, so a state set
/// never holds a duplicate however many seeds reach it. Both the simulation and
/// the subset construction in [`dfa`] call this, so there is one definition of
/// a closure rather than two that could drift apart.
fn add_thread(
    instructions: &[Instruction],
    active: &mut Vec<usize>,
    marks: &mut [u64],
    pending: &mut Vec<usize>,
    generation: u64,
    start: usize,
) {
    pending.clear();
    pending.push(start);
    while let Some(index) = pending.pop() {
        if marks.get(index).copied() == Some(generation) {
            continue;
        }
        if let Some(mark) = marks.get_mut(index) {
            *mark = generation;
        }
        match instructions.get(index) {
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

/// A compiled pattern that scans input in a single left-to-right pass.
///
/// Matching considers every alternative at once, so run time is linear in the
/// input length and no input can trigger exponential backtracking. Which engine
/// does the considering is chosen once, at compile time; see the module
/// documentation.
#[derive(Debug)]
pub struct Pattern {
    instructions: Vec<Instruction>,
    matchers: Vec<Matcher>,
    anchored_start: bool,
    anchored_end: bool,
    engine: Engine,
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

        // A literal set only answers for the whole value, so it needs both
        // anchors; a pattern anchored at one end is a prefix or suffix test the
        // automaton handles instead.
        let literals = (anchored_start && anchored_end)
            .then(|| literal_language(&node))
            .flatten();
        let engine = if let Some(literals) = literals {
            Engine::Literals(LiteralSet::new(literals))
        } else if let Some(automaton) = dfa::Dfa::build(
            &program.instructions,
            &program.matchers,
            anchored_start,
            anchored_end,
        ) {
            Engine::Deterministic(Box::new(automaton))
        } else {
            Engine::Simulation
        };

        Ok(Self {
            instructions: program.instructions,
            matchers: program.matchers,
            anchored_start,
            anchored_end,
            engine,
        })
    }

    /// Reports whether the value satisfies the pattern.
    ///
    /// The work this does depends on the shape the pattern compiled to. A
    /// doubly anchored literal alternation is a length test and an equality
    /// test; an ASCII pattern is one table lookup per input byte; anything else
    /// runs the simulation described on [`Pattern::matches_simulated`].
    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match &self.engine {
            Engine::Literals(literals) => literals.matches(value),
            Engine::Deterministic(automaton) => automaton.matches(value),
            Engine::Simulation => self.matches_simulated(value),
        }
    }

    /// Reports whether the value satisfies the pattern, always by simulation.
    ///
    /// This is the definition the specialized engines are held to: the module's
    /// differential tests run this and [`Pattern::matches`] over the same
    /// inputs and require identical answers, and the matcher benchmark uses it
    /// as the baseline column. Prefer [`Pattern::matches`] everywhere else.
    ///
    /// The simulation advances every live alternative one input character at a
    /// time, on per-thread scratch buffers rather than fresh allocations. It
    /// used to allocate a mark table and an active set per call, plus one
    /// successor set *per input character* and one work list *per state added*:
    /// matching a thirteen-character slug cost dozens of allocations, and a
    /// bulk request validating fifty items against two pattern rules cost
    /// thousands. Under four worker threads that turned into allocator
    /// contention rather than work, and it showed as a server that used 117% of
    /// one core where its peers used 250-280% on the same request, and that got
    /// *slower* going from one connection to sixty-four.
    #[must_use]
    pub fn matches_simulated(&self, value: &str) -> bool {
        SCRATCH.with_borrow_mut(|scratch| self.simulate(value, scratch))
    }

    fn simulate(&self, value: &str, scratch: &mut Scratch) -> bool {
        let Scratch {
            marks,
            active,
            next,
            pending,
            generation,
        } = scratch;
        if marks.len() < self.instructions.len() {
            marks.resize(self.instructions.len(), 0);
        }
        if *generation == u64::MAX {
            // The next epoch would repeat one already stamped into the marks.
            // Reaching this needs about six hundred years of matching, but a
            // wrong answer is not something to leave to arithmetic luck.
            marks.fill(0);
            *generation = 0;
        }
        active.clear();
        next.clear();

        *generation += 1;
        add_thread(&self.instructions, active, marks, pending, *generation, 0);

        for character in value.chars() {
            if !self.anchored_end && self.accepts(active) {
                return true;
            }
            *generation += 1;
            next.clear();
            for &index in active.iter() {
                if let Some(Instruction::Consume(matcher)) = self.instructions.get(index)
                    && self
                        .matchers
                        .get(*matcher)
                        .is_some_and(|matcher| matcher.contains(character))
                {
                    add_thread(
                        &self.instructions,
                        next,
                        marks,
                        pending,
                        *generation,
                        index + 1,
                    );
                }
            }
            if !self.anchored_start {
                add_thread(&self.instructions, next, marks, pending, *generation, 0);
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
    /// [`Pattern::matches`] may borrow a thread-local scratch set, so a caller
    /// already inside that borrow cannot call it again. Nothing in this crate
    /// does, but a future matcher that recursed would deadlock rather than
    /// misbehave quietly, and this entry point is the way out.
    #[must_use]
    pub fn matches_isolated(&self, value: &str) -> bool {
        match &self.engine {
            Engine::Literals(literals) => literals.matches(value),
            Engine::Deterministic(automaton) => automaton.matches(value),
            Engine::Simulation => self.simulate(value, &mut Scratch::default()),
        }
    }

    fn accepts(&self, active: &[usize]) -> bool {
        active
            .iter()
            .any(|index| matches!(self.instructions.get(*index), Some(Instruction::Accept)))
    }

    /// Name of the engine this pattern compiled to, for tests that assert a
    /// specialization was actually found.
    #[cfg(test)]
    const fn engine_label(&self) -> &'static str {
        match &self.engine {
            Engine::Literals(_) => "literals",
            Engine::Deterministic(_) => "deterministic",
            Engine::Simulation => "simulation",
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

    /// Patterns spanning every shape the engine choice turns on.
    ///
    /// Each entry pairs a pattern with the engine it is expected to compile to,
    /// so a change that silently drops a specialization fails here rather than
    /// only showing up as a slower benchmark.
    const ENGINE_CASES: &[(&str, &str)] = &[
        // The two rules `POST /ingest/articles/bulk` evaluates, a hundred times
        // per request.
        ("^(uk|ru|en)$", "literals"),
        ("^[a-z0-9]+(-[a-z0-9]+)*$", "deterministic"),
        // Literal shapes.
        ("^draft$", "literals"),
        ("^(GET|POST|PUT|PATCH|DELETE)$", "literals"),
        ("^(a|b)(c|d)$", "literals"),
        ("^$", "literals"),
        ("^(a|)$", "literals"),
        (r"^v1\.2$", "literals"),
        // Anchored at one end only, so the literal set does not apply.
        ("^draft", "deterministic"),
        ("draft$", "deterministic"),
        // ASCII classes and quantifiers.
        ("^[a-z][a-z0-9_]*$", "deterministic"),
        (r"^\d+$", "deterministic"),
        ("^[0-9a-f]*$", "deterministic"),
        (r"^(cat|dog|bird)-\d+$", "deterministic"),
        ("^colou?r$", "deterministic"),
        ("^(a+)+$", "deterministic"),
        (r"\d+", "deterministic"),
        (r"^\w+\s\w+$", "deterministic"),
        ("^[a-z]+@[a-z]+[.](com|net|org)$", "deterministic"),
        // Shapes that can accept a character outside ASCII, so byte scanning is
        // unsound and the simulation must be kept.
        ("^a.b$", "simulation"),
        ("^a[^0-9]$", "simulation"),
        (r"^\D+$", "simulation"),
        (r"^\W$", "simulation"),
        (r"^\S+$", "simulation"),
        ("^[а-я]+$", "simulation"),
        ("^(да|ні)$", "literals"),
        ("^日本語$", "literals"),
    ];

    /// Values chosen to exercise the boundaries the engines differ at: the
    /// empty string, ASCII edges, and multi-byte UTF-8 both alone and mixed
    /// into otherwise matching input.
    const DIFFERENTIAL_VALUES: &[&str] = &[
        "",
        "a",
        "ab",
        "uk",
        "ru",
        "en",
        "de",
        "UK",
        "uk ",
        " uk",
        "ukr",
        "draft",
        "drafts",
        "GET",
        "PATCH",
        "ac",
        "bd",
        "ad",
        "ingested-0000",
        "Ingested 0000",
        "a-record-quarter-for-northern-logistics",
        "-leading",
        "trailing-",
        "double--dash",
        "user_42",
        "4user",
        "cat-1",
        "bird-9001",
        "fish-1",
        "cat-",
        "color",
        "colour",
        "v1.2",
        "v1x2",
        "a\tb",
        "a\nb",
        "a b",
        "hello world",
        "0123456789",
        "deadbeef",
        "user@example.com",
        "user@example.zz",
        "\0",
        "\u{7f}",
        // Multi-byte UTF-8, alone and adjacent to bytes that would otherwise
        // match. The continuation bytes of these characters are the ones a
        // byte-level scan must not mistake for anything.
        "é",
        "aé",
        "éa",
        "ingested-é",
        "é-ingested",
        "да",
        "ні",
        "ru\u{0301}",
        "日本語",
        "日本",
        "日本語です",
        "🦀",
        "a🦀b",
        "uk🦀",
        "\u{80}\u{80}",
        "\u{10ffff}",
        "Ω",
        "ΩΩΩ",
        "тест-123",
    ];

    /// Runs both engines over one pattern and one value and requires agreement.
    fn agree(pattern: &Pattern, source: &str, value: &str) {
        assert_eq!(
            pattern.matches(value),
            pattern.matches_simulated(value),
            "engine `{}` disagreed with the simulation on pattern {source:?} and value {value:?}",
            pattern.engine_label()
        );
        assert_eq!(
            pattern.matches_isolated(value),
            pattern.matches_simulated(value),
            "the isolated entry point disagreed with the simulation on pattern {source:?} \
             and value {value:?}"
        );
    }

    #[test]
    fn every_pattern_shape_compiles_to_the_engine_it_is_meant_to() {
        for (source, expected) in ENGINE_CASES {
            let pattern = Pattern::compile(source).expect("the fixture pattern compiles");
            assert_eq!(
                pattern.engine_label(),
                *expected,
                "pattern {source:?} chose an unexpected engine"
            );
        }
    }

    #[test]
    fn specialized_engines_answer_exactly_as_the_simulation_does() {
        for (source, _) in ENGINE_CASES {
            let pattern = Pattern::compile(source).expect("the fixture pattern compiles");
            for value in DIFFERENTIAL_VALUES {
                agree(&pattern, source, value);
            }
        }
    }

    /// A trailing multi-byte character must not turn a rejection into a match,
    /// and a leading one must not turn a match into a rejection. Both are
    /// failure modes a byte-level scan invents and a character-level one
    /// cannot, so they get a case of their own rather than only living in the
    /// matrix above.
    #[test]
    fn multi_byte_input_cannot_satisfy_an_ascii_only_pattern() {
        for source in ["^[a-z0-9]+(-[a-z0-9]+)*$", r"^\w+$", r"\d+", "^[a-z]+"] {
            let pattern = Pattern::compile(source).expect("the fixture pattern compiles");
            for value in [
                "ingested-0000é",
                "éingested-0000",
                "ingested-é-0000",
                "🦀",
                "日本語",
                "\u{c3}\u{a9}",
            ] {
                agree(&pattern, source, value);
            }
        }
    }

    /// Deterministic string generator, so a failure is reproducible from the
    /// seed alone and the suite never becomes flaky.
    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> u64 {
            // xorshift64*, chosen because it is four lines and needs no
            // dependency; the sequence only has to be varied, not random.
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn value(&mut self) -> String {
            const ALPHABET: &[char] = &[
                'a', 'b', 'z', '0', '9', '-', '_', '.', '|', ' ', '\t', '\n', 'A', 'Z', 'é', 'я',
                '日', '🦀',
            ];
            let length = usize::try_from(self.next() % 12).unwrap_or(0);
            (0..length)
                .map(|_| {
                    let index = usize::try_from(self.next()).unwrap_or(0) % ALPHABET.len();
                    ALPHABET[index]
                })
                .collect()
        }
    }

    #[test]
    fn generated_values_never_split_the_engines() {
        let mut noise = Noise(0x9e37_79b9_7f4a_7c15);
        let patterns = ENGINE_CASES
            .iter()
            .map(|(source, _)| {
                (
                    *source,
                    Pattern::compile(source).expect("the fixture pattern compiles"),
                )
            })
            .collect::<Vec<_>>();

        for _ in 0..2_000 {
            let value = noise.value();
            for (source, pattern) in &patterns {
                agree(pattern, source, &value);
            }
        }
    }

    /// The linear-time guarantee against hostile *input* is why this matcher
    /// exists instead of a backtracking one, and it has to survive every engine
    /// the fast paths added.
    #[test]
    fn hostile_input_stays_linear_under_every_engine() {
        for (source, expected) in [
            ("^(a+)+$", "deterministic"),
            ("^(a|a)*$", "deterministic"),
            ("^(a|aa)+b$", "deterministic"),
            ("^(a.)*b$", "simulation"),
        ] {
            let pattern = Pattern::compile(source).expect("the fixture pattern compiles");
            assert_eq!(pattern.engine_label(), expected, "pattern {source:?}");
            for length in [64_usize, 4_096] {
                let accepted = "a".repeat(length);
                let rejected = accepted.clone() + "b";
                agree(&pattern, source, &accepted);
                agree(&pattern, source, &rejected);
            }
        }
    }

    /// A pattern whose subset construction would exceed the bounds must keep
    /// the simulation rather than build an unbounded table.
    #[test]
    fn an_exploding_pattern_falls_back_instead_of_growing_a_table() {
        // "an `a` somewhere, then exactly ten more letters". The thread set has
        // to remember which of the last eleven positions held an `a`, so the
        // deterministic form needs a state per subset of them while the
        // simulation stays the size of the program. This is the shape the state
        // bound exists for.
        let source = format!("^[a-z]*a{}$", "[a-z]".repeat(10));
        let pattern = Pattern::compile(&source).expect("the fixture pattern compiles");
        assert_eq!(
            pattern.engine_label(),
            "simulation",
            "a pattern past the state bound must fall back"
        );

        let accepted = format!("a{}", "b".repeat(10));
        assert!(pattern.matches(&accepted));
        assert_eq!(
            pattern.matches(&accepted),
            pattern.matches_simulated(&accepted)
        );
        for rejected in [
            "b".repeat(11),
            "a".repeat(10),
            String::new(),
            "aé".to_owned(),
        ] {
            assert!(!pattern.matches(&rejected), "must reject {rejected:?}");
            agree(&pattern, &source, &rejected);
        }
    }

    #[test]
    fn scratch_buffers_shared_by_several_patterns_do_not_leak_marks() {
        // One thread's buffers serve every pattern it evaluates, and the marks
        // are stamped rather than cleared. Interleaving patterns of different
        // program sizes is what would expose a stale stamp.
        let short = Pattern::compile("^a.c$").expect("pattern compiles");
        let long = Pattern::compile("^(alpha|beta).(gamma|delta)+$").expect("pattern compiles");
        for _ in 0..64 {
            assert!(short.matches_simulated("abc"));
            assert!(!short.matches_simulated("abcd"));
            assert!(long.matches_simulated("alpha-gamma"));
            assert!(!long.matches_simulated("alpha-omega"));
        }
    }
}

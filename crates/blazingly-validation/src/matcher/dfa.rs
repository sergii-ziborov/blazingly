//! Deterministic byte-level matching for the ASCII subset of compiled patterns.
//!
//! The Thompson simulation in the parent module walks a set of live threads and
//! recomputes an epsilon closure for every input character. Every answer it can
//! give is a function of that thread set alone, so the sets reachable from the
//! start form a finite automaton that can be enumerated once at compile time.
//! Matching then costs one table lookup per input byte instead of one closure
//! per input character.
//!
//! Two properties are preserved exactly.
//!
//! *Linear time in the input.* The construction is bounded: a pattern whose
//! subset automaton would exceed [`MAX_STATES`], [`MAX_CLASSES`], or
//! [`MAX_TRANSITIONS`] is not built here at all, and the caller keeps the
//! simulation. Nothing is built while matching, so no input can make the
//! matcher do more than one lookup per byte, and no pattern can make it
//! allocate an unbounded table.
//!
//! *Answers identical to the simulation.* Transitions are computed by running
//! the same closure routine the simulation runs, over the same instructions, so
//! there is one definition of a state set rather than two. The differential
//! tests in the parent module check both engines against each other.
//!
//! Bytes, not characters. Scanning `&str` as bytes skips UTF-8 decoding, which
//! the simulation pays on every step. It is only sound when every character
//! class in the pattern is ASCII-only, because then no character outside ASCII
//! can be accepted by anything: a multi-byte character kills every live thread,
//! and so does each of its bytes taken separately, leaving the same state set
//! either way. [`classify`] refuses to build the byte classifier for any pattern
//! that could accept a non-ASCII character, so this module never sees one.

use super::{Instruction, Matcher, add_thread};
use std::collections::HashMap;

// The four bounds below cap construction cost as well as table size, because
// `check_pattern` may be handed pattern text built at run time and the
// compilation cache holds only [`PATTERN_CACHE_CAPACITY`](crate::PATTERN_CACHE_CAPACITY)
// entries: a caller cycling through more patterns than that pays a construction
// on every call. Together they hold the worst case to roughly a millisecond of
// one-off work and eight kibibytes of table, while leaving every pattern a
// declarative field rule realistically states far inside them — the two rules
// the API benchmark declares build four states over four columns.

/// Largest number of deterministic states built before giving up.
const MAX_STATES: usize = 160;

/// Largest number of distinct byte classes a transition table may span.
const MAX_CLASSES: usize = 64;

/// Largest transition table, in entries. Entries are `u16`, so this caps one
/// compiled pattern's table at eight kibibytes.
const MAX_TRANSITIONS: usize = 4096;

/// Largest compiled program the construction is attempted on.
const MAX_PROGRAM: usize = 160;

/// Largest total character-class membership the byte classifier is built from.
///
/// Classification tests every matcher against all 256 byte values, so this is
/// what bounds that pass. Four maximal classes fit, and
/// [`MAX_CLASS_MEMBERS`](crate::MAX_CLASS_MEMBERS) bounds each one.
const MAX_CLASSIFIER_MEMBERS: usize = 512;

/// The empty thread set. No input revives it, so reaching it ends the match.
const DEAD: usize = 0;

/// Byte values grouped by the set of character classes that accept them.
///
/// Two bytes that every class in the pattern treats alike drive the automaton
/// identically, so they share one column of the transition table. A pattern
/// over `[a-z0-9]` and `-` has three columns rather than two hundred and
/// fifty-six.
struct ByteClasses {
    /// Column index of each byte value.
    of_byte: [u8; 256],
    /// One byte drawn from each column, used to evaluate the classes while the
    /// automaton is being built.
    representatives: Vec<u8>,
}

/// Groups byte values by class membership, or reports that the pattern is not
/// ASCII-only.
///
/// The partition is refined one matcher at a time: every byte starts in one
/// class, and each matcher splits each existing class into the bytes it accepts
/// and the bytes it does not. Two bytes end up apart exactly when some matcher
/// separates them, which is the partition a per-byte membership signature would
/// describe — reached without building, hashing, and comparing one signature
/// per byte value.
fn classify(matchers: &[Matcher]) -> Option<ByteClasses> {
    if !matchers.iter().all(Matcher::is_ascii_only) {
        return None;
    }
    if matchers.iter().map(Matcher::member_count).sum::<usize>() > MAX_CLASSIFIER_MEMBERS {
        return None;
    }

    let mut of_byte = [0_u8; 256];
    let mut count = 1_usize;
    let mut moved: Vec<Option<u8>> = Vec::new();
    for matcher in matchers {
        // Classes present before this matcher runs. A byte is visited once per
        // pass, so the class read below is always one of these, never one this
        // pass just created.
        let existing = count;
        moved.clear();
        moved.resize(existing, None);
        for byte in 0..=u8::MAX {
            // Byte values at or above `0x80` never stand for a character on
            // their own; they only ever appear inside a multi-byte encoding.
            // Testing the Latin-1 character of the same value is what lands
            // them in the reject class, which is where a non-ASCII character
            // belongs once every class is known to be ASCII-only.
            if !matcher.contains(char::from(byte)) {
                continue;
            }
            let current = usize::from(of_byte[usize::from(byte)]);
            let destination = if let Some(destination) = moved.get(current).copied().flatten() {
                destination
            } else {
                if count >= MAX_CLASSES {
                    return None;
                }
                let destination = u8::try_from(count).ok()?;
                count += 1;
                if let Some(slot) = moved.get_mut(current) {
                    *slot = Some(destination);
                }
                destination
            };
            of_byte[usize::from(byte)] = destination;
        }
    }

    // Splitting can empty a class, when every byte in it moved together. Renumber
    // so the class indices are dense, which is what lets them index the
    // transition table directly.
    let mut compacted = vec![None::<u8>; count];
    let mut representatives = Vec::new();
    for byte in 0..=u8::MAX {
        let class = usize::from(of_byte[usize::from(byte)]);
        let dense = if let Some(dense) = compacted.get(class).copied().flatten() {
            dense
        } else {
            let dense = u8::try_from(representatives.len()).ok()?;
            if let Some(slot) = compacted.get_mut(class) {
                *slot = Some(dense);
            }
            representatives.push(byte);
            dense
        };
        of_byte[usize::from(byte)] = dense;
    }

    Some(ByteClasses {
        of_byte,
        representatives,
    })
}

/// A deterministic automaton over byte classes.
#[derive(Debug)]
pub(super) struct Dfa {
    /// Column index of each byte value.
    of_byte: [u8; 256],
    /// Row-major transitions, indexed by `(state << stride_shift) | class`.
    ///
    /// Rows are padded to a power of two so the row base is a shift rather than
    /// a multiply, which takes the multiplier off the loop-carried dependency
    /// chain between one byte and the next.
    transitions: Box<[u16]>,
    /// Whether each state's thread set contains the accepting instruction.
    accepting: Box<[bool]>,
    /// Row width, as a shift amount.
    stride_shift: u32,
    /// State the scan begins in.
    start: u16,
    /// Whether the pattern is anchored at the end of the input.
    anchored_end: bool,
}

impl Dfa {
    /// Builds the automaton, or reports that this pattern must keep the
    /// simulation.
    pub(super) fn build(
        instructions: &[Instruction],
        matchers: &[Matcher],
        anchored_start: bool,
        anchored_end: bool,
    ) -> Option<Self> {
        if instructions.len() > MAX_PROGRAM {
            return None;
        }
        let classes = classify(matchers)?;
        let class_count = classes.representatives.len();
        let stride_shift = class_count.next_power_of_two().trailing_zeros();
        let stride = 1_usize << stride_shift;

        let mut builder = Builder {
            sets: vec![Vec::new()],
            identity: HashMap::from([(Vec::new(), 0_u16)]),
            marks: vec![0_u64; instructions.len()],
            pending: Vec::new(),
            generation: 0,
        };

        let start_set = builder.closure(instructions, &[0]);
        let start = builder.register(start_set)?;

        // The dead state's row. Every column stays dead, so a rejected byte
        // ends the scan without any special case in the matching loop.
        let mut transitions = vec![0_u16; stride];
        let mut state = 1_usize;
        while state < builder.sets.len() {
            for class in 0..stride {
                let reached = match classes.representatives.get(class) {
                    // Padding beyond the last real class. No byte maps to it,
                    // so the column is never read.
                    None => Vec::new(),
                    Some(representative) => {
                        let representative = char::from(*representative);
                        let mut seeds = Vec::new();
                        for &index in &builder.sets[state] {
                            if let Some(Instruction::Consume(matcher)) = instructions.get(index)
                                && matchers
                                    .get(*matcher)
                                    .is_some_and(|matcher| matcher.contains(representative))
                            {
                                seeds.push(index + 1);
                            }
                        }
                        // An unanchored pattern restarts at every position, so
                        // the start instruction stays live throughout.
                        if !anchored_start {
                            seeds.push(0);
                        }
                        builder.closure(instructions, &seeds)
                    }
                };
                transitions.push(builder.register(reached)?);
            }
            if transitions.len() > MAX_TRANSITIONS {
                return None;
            }
            state += 1;
        }

        let accepting = builder
            .sets
            .iter()
            .map(|set| {
                set.iter()
                    .any(|index| matches!(instructions.get(*index), Some(Instruction::Accept)))
            })
            .collect::<Vec<_>>();

        Some(Self {
            of_byte: classes.of_byte,
            transitions: transitions.into_boxed_slice(),
            accepting: accepting.into_boxed_slice(),
            stride_shift,
            start,
            anchored_end,
        })
    }

    /// Reports whether the value satisfies the pattern.
    pub(super) fn matches(&self, value: &str) -> bool {
        let mut state = usize::from(self.start);
        if self.anchored_end {
            for &byte in value.as_bytes() {
                state = self.step(state, byte);
                if state == DEAD {
                    return false;
                }
            }
        } else {
            // Without an end anchor the match may finish at any position, so
            // acceptance is tested at every byte boundary, exactly where the
            // simulation tests it.
            for &byte in value.as_bytes() {
                if self.accepts(state) {
                    return true;
                }
                state = self.step(state, byte);
                if state == DEAD {
                    return false;
                }
            }
        }
        self.accepts(state)
    }

    #[inline]
    fn step(&self, state: usize, byte: u8) -> usize {
        let class = usize::from(self.of_byte[usize::from(byte)]);
        self.transitions
            .get((state << self.stride_shift) | class)
            .map_or(DEAD, |next| usize::from(*next))
    }

    #[inline]
    fn accepts(&self, state: usize) -> bool {
        self.accepting.get(state).copied().unwrap_or(false)
    }
}

/// Subset-construction bookkeeping.
struct Builder {
    /// Thread sets discovered so far, indexed by state number. Index zero is
    /// the empty set.
    sets: Vec<Vec<usize>>,
    /// State number of each discovered thread set.
    identity: HashMap<Vec<usize>, u16>,
    /// Closure marks, shared across every closure taken during construction.
    marks: Vec<u64>,
    /// Closure work list.
    pending: Vec<usize>,
    /// Epoch distinguishing one closure's marks from the next.
    generation: u64,
}

impl Builder {
    /// Takes the epsilon closure of the seeds, canonically ordered.
    fn closure(&mut self, instructions: &[Instruction], seeds: &[usize]) -> Vec<usize> {
        self.generation += 1;
        let mut set = Vec::new();
        for &seed in seeds {
            add_thread(
                instructions,
                &mut set,
                &mut self.marks,
                &mut self.pending,
                self.generation,
                seed,
            );
        }
        set.sort_unstable();
        set
    }

    /// Returns the state number of a thread set, assigning one if it is new.
    fn register(&mut self, set: Vec<usize>) -> Option<u16> {
        if let Some(state) = self.identity.get(&set) {
            return Some(*state);
        }
        if self.sets.len() >= MAX_STATES {
            return None;
        }
        let state = u16::try_from(self.sets.len()).ok()?;
        self.identity.insert(set.clone(), state);
        self.sets.push(set);
        Some(state)
    }
}

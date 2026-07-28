//! Reuse of compiled `#[pattern("...")]` expressions across requests.
//!
//! A declarative pattern rule is a fixed string chosen when the model is
//! written, but the check that enforces it runs once per request. Compiling the
//! expression on every call made the compile cost proportional to traffic
//! instead of proportional to the number of declared rules. This module keeps
//! each distinct pattern compiled and hands the compiled form back on later
//! calls.
//!
//! The cache lives in thread-local storage. A worker thread therefore never
//! synchronizes with another worker to evaluate a field rule, which is the
//! whole point of caching the compilation in the first place. Compiled patterns
//! are small and the set of declared patterns is fixed, so duplicating them per
//! worker costs far less than one lock acquisition per request would.
//!
//! Entries are keyed on the pattern *text*, not on the address of the string
//! that carried it. [`check_pattern`](crate::check_pattern) accepts any
//! `&str`, so a caller may pass pattern text that was built at run time and
//! then freed; a later allocation can reuse that address while holding
//! different bytes. Comparing the text is a few nanoseconds for the short
//! strings a field rule declares, and it removes that failure mode entirely.
//! Keying on text also means the same pattern declared by several models
//! shares one compilation.

use crate::matcher::{Pattern, PatternError};
use std::cell::RefCell;
use std::rc::Rc;

/// Largest number of distinct patterns kept compiled on one thread.
///
/// Declarative field rules cannot reach this bound: a binary contains a fixed
/// set of `#[pattern("...")]` literals. The bound exists because
/// [`check_pattern`](crate::check_pattern) is callable with pattern text built
/// at run time, and an unbounded cache would let such a caller pin memory
/// without limit. Once the cache is full the oldest entry is dropped, so a
/// caller that cycles through more than this many patterns falls back to the
/// uncached cost rather than growing the cache.
pub const PATTERN_CACHE_CAPACITY: usize = 64;

/// One memoized compilation, successful or rejected.
///
/// Rejections are cached too. An unusable pattern reports the same violation on
/// every request, and re-running the parser to rediscover that would leave the
/// slowest path uncached.
struct Entry {
    source: Box<str>,
    compiled: Result<Rc<Pattern>, PatternError>,
}

thread_local! {
    /// Compilations reused by every pattern rule evaluated on this thread.
    static COMPILED: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

/// Returns the compiled form of `pattern`, compiling it at most once per thread.
///
/// The compiled pattern is handed back behind an [`Rc`] so the cache borrow is
/// released before the caller matches anything against it. Nothing the caller
/// does with the result can therefore re-enter the cache while it is borrowed.
pub(crate) fn compiled_pattern(pattern: &str) -> Result<Rc<Pattern>, PatternError> {
    COMPILED
        .try_with(|cache| {
            let mut entries = cache.borrow_mut();
            let hit = entries.iter().find_map(|entry| {
                (entry.source.as_ref() == pattern).then(|| entry.compiled.clone())
            });
            if let Some(compiled) = hit {
                return compiled;
            }

            let compiled = Pattern::compile(pattern).map(Rc::new);
            if entries.len() >= PATTERN_CACHE_CAPACITY {
                entries.remove(0);
            }
            entries.push(Entry {
                source: Box::from(pattern),
                compiled: compiled.clone(),
            });
            compiled
        })
        // Thread-local storage is already destroyed while a thread is being
        // torn down. Compiling directly keeps the check correct there instead
        // of turning a late validation into a panic.
        .unwrap_or_else(|_| Pattern::compile(pattern).map(Rc::new))
}

/// Number of patterns currently cached on this thread.
#[cfg(test)]
fn cached_count() -> usize {
    COMPILED.with(|cache| cache.borrow().len())
}

#[cfg(test)]
mod tests {
    use super::{PATTERN_CACHE_CAPACITY, cached_count, compiled_pattern};
    use std::rc::Rc;

    fn compiled(pattern: &str) -> Rc<super::Pattern> {
        compiled_pattern(pattern).expect("pattern compiles")
    }

    #[test]
    fn a_repeated_pattern_is_compiled_once() {
        let first = compiled("^[a-z0-9]+(-[a-z0-9]+)*$");
        let second = compiled("^[a-z0-9]+(-[a-z0-9]+)*$");
        assert!(
            Rc::ptr_eq(&first, &second),
            "the second call must reuse the first compilation"
        );
        assert!(second.matches("hello-world-42"));
    }

    #[test]
    fn two_patterns_alternating_through_one_call_site_stay_distinct() {
        // A single `check_pattern` call site is shared by every pattern rule in
        // the binary, so the cache must not collapse to one memoized slot.
        let slug = compiled("^[a-z0-9]+(-[a-z0-9]+)*$");
        let lang = compiled("^(uk|ru|en)$");
        assert!(!Rc::ptr_eq(&slug, &lang));

        for _ in 0..4 {
            let slug_again = compiled("^[a-z0-9]+(-[a-z0-9]+)*$");
            let lang_again = compiled("^(uk|ru|en)$");
            assert!(Rc::ptr_eq(&slug, &slug_again));
            assert!(Rc::ptr_eq(&lang, &lang_again));
            assert!(slug_again.matches("hello-world"));
            assert!(!slug_again.matches("Hello World"));
            assert!(lang_again.matches("uk"));
            assert!(!lang_again.matches("hello-world"));
        }
    }

    #[test]
    fn equal_text_from_unrelated_strings_shares_one_compilation() {
        // Two models declaring the same rule may hand over text that lives at
        // different addresses. Keying on the text, not the address, is what
        // makes those share a compilation.
        let owned = String::from("^(uk|ru|en)$");
        let rebuilt = format!("^({}|{}|{})$", "uk", "ru", "en");
        assert_ne!(
            owned.as_ptr(),
            rebuilt.as_ptr(),
            "the fixture must use two distinct allocations"
        );

        let literal = compiled("^(uk|ru|en)$");
        assert!(Rc::ptr_eq(&literal, &compiled(&owned)));
        assert!(Rc::ptr_eq(&literal, &compiled(&rebuilt)));
    }

    #[test]
    fn text_freed_and_reallocated_never_answers_with_another_pattern() {
        // Run-time pattern text can be dropped and its allocation handed to the
        // next pattern of the same size. An address-keyed cache would then
        // answer the second lookup with the first pattern's program.
        for (source, sample, counterexample) in [
            ("^a+$", "aaa", "bbb"),
            ("^b+$", "bbb", "ccc"),
            ("^c+$", "ccc", "aaa"),
        ] {
            let owned = String::from(source);
            let pattern = compiled(&owned);
            drop(owned);
            assert!(pattern.matches(sample), "{source} must accept {sample}");
            assert!(
                !pattern.matches(counterexample),
                "{source} must reject {counterexample}"
            );
        }
    }

    #[test]
    fn a_rejected_pattern_is_remembered_instead_of_reparsed() {
        let before = cached_count();
        let first = compiled_pattern("^a{2}$").expect_err("counted repetition is unsupported");
        let after_first = cached_count();
        let second = compiled_pattern("^a{2}$").expect_err("counted repetition is unsupported");
        assert_eq!(first, second);
        assert!(
            after_first <= before + 1,
            "the rejection occupies at most one entry"
        );
        assert_eq!(
            cached_count(),
            after_first,
            "the second lookup must hit the cached rejection"
        );
    }

    #[test]
    fn the_cache_stops_growing_at_its_capacity() {
        for index in 0..PATTERN_CACHE_CAPACITY + 8 {
            let pattern = format!("^p{index}[a-z]*$");
            assert!(compiled(&pattern).matches(&format!("p{index}abc")));
        }
        assert_eq!(
            cached_count(),
            PATTERN_CACHE_CAPACITY,
            "distinct run-time patterns must not grow the cache without bound"
        );
    }
}

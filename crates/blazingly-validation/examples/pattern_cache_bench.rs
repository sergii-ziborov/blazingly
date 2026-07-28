//! Per-call cost of a `#[pattern("...")]` field rule, compiled per call versus
//! compiled once and reused.
//!
//! The two patterns below are the ones a `POST /admin/articles` body carries in
//! the API benchmark: `slug` and `lang` are both required strings, so a single
//! request evaluates both rules.
//!
//! Method. The two variants are sampled alternately in short bursts, so a
//! frequency change or a burst of unrelated host load lands on both columns
//! rather than on one of them. Each series reports the fastest of its bursts:
//! no other process on the host can make this loop run faster than it really
//! is, so on a machine that is never idle the minimum is the sample least
//! polluted by interference. The median is printed beside it to keep the spread
//! visible.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p blazingly-validation --example pattern_cache_bench -j 3
//! ```

use blazingly_core::ValidationErrors;
use blazingly_validation::{Pattern, check_pattern};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Iterations per burst. Short enough to fit inside one scheduling quantum,
/// long enough to dwarf the clock's resolution.
const ITERATIONS: u32 = 500;

/// Bursts taken per series.
const BURSTS: usize = 400;

/// Iterations discarded before sampling, so the cache and the branch predictors
/// are in their steady state.
const WARMUP: u32 = 5_000;

/// `CreateArticle::slug`.
const SLUG: &str = "^[a-z0-9]+(-[a-z0-9]+)*$";

/// `CreateArticle::lang`.
const LANG: &str = "^(uk|ru|en)$";

const SLUG_VALUE: &str = "a-record-quarter-for-northern-logistics";
const LANG_VALUE: &str = "uk";

/// One rule evaluated the way `check_pattern` did before the cache existed.
fn uncached(value: &str, pattern: &str) -> bool {
    Pattern::compile(pattern).is_ok_and(|compiled| compiled.matches(value))
}

/// Fastest and median per-call duration of one series.
#[derive(Clone, Copy)]
struct Cost {
    fastest: Duration,
    median: Duration,
}

impl Cost {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self {
            fastest: samples[0],
            median: samples[samples.len() / 2],
        }
    }
}

fn burst(body: &mut impl FnMut()) -> Duration {
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        body();
    }
    started.elapsed() / ITERATIONS
}

/// Samples both variants alternately and reports one cost for each.
fn compare(mut before: impl FnMut(), mut after: impl FnMut()) -> (Cost, Cost) {
    for _ in 0..WARMUP {
        before();
        after();
    }
    let mut before_samples = Vec::with_capacity(BURSTS);
    let mut after_samples = Vec::with_capacity(BURSTS);
    for _ in 0..BURSTS {
        before_samples.push(burst(&mut before));
        after_samples.push(burst(&mut after));
    }
    (Cost::of(before_samples), Cost::of(after_samples))
}

fn report(label: &str, before: Cost, after: Cost) {
    let saved = before.fastest.saturating_sub(after.fastest);
    let ratio = before.fastest.as_secs_f64() / after.fastest.as_secs_f64();
    let (before_fastest, after_fastest) = (before.fastest, after.fastest);
    let (before_median, after_median) = (before.median, after.median);
    println!(
        "{label:<24} {before_fastest:>10.2?} {after_fastest:>10.2?} {saved:>10.2?} {ratio:>7.2}x  \
         {before_median:>10.2?} {after_median:>10.2?}"
    );
}

fn main() {
    println!("{ITERATIONS} iterations x {BURSTS} interleaved bursts per series");
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>8}  {:>10} {:>10}",
        "", "before", "after", "saved", "faster", "before", "after"
    );
    println!(
        "{:<24} {:>10} {:>10} {:>10} {:>8}  {:>10} {:>10}",
        "rule", "fastest", "fastest", "", "", "median", "median"
    );

    let mut errors = ValidationErrors::new();

    let (slug_before, slug_after) = compare(
        || {
            black_box(uncached(black_box(SLUG_VALUE), black_box(SLUG)));
        },
        || {
            check_pattern(&mut errors, "slug", black_box(SLUG_VALUE), black_box(SLUG));
        },
    );
    report("slug rule", slug_before, slug_after);

    let (lang_before, lang_after) = compare(
        || {
            black_box(uncached(black_box(LANG_VALUE), black_box(LANG)));
        },
        || {
            check_pattern(&mut errors, "lang", black_box(LANG_VALUE), black_box(LANG));
        },
    );
    report("lang rule", lang_before, lang_after);

    // Both rules, in the order one `POST /admin/articles` body evaluates them.
    let (request_before, request_after) = compare(
        || {
            black_box(uncached(black_box(SLUG_VALUE), black_box(SLUG)));
            black_box(uncached(black_box(LANG_VALUE), black_box(LANG)));
        },
        || {
            check_pattern(&mut errors, "slug", black_box(SLUG_VALUE), black_box(SLUG));
            check_pattern(&mut errors, "lang", black_box(LANG_VALUE), black_box(LANG));
        },
    );
    report("POST /admin/articles", request_before, request_after);

    assert!(
        errors.is_empty(),
        "the benchmark values must satisfy both rules"
    );

    // The eliminated work on its own, matching nothing in either column.
    let (slug_compile, lang_compile) = compare(
        || {
            black_box(Pattern::compile(black_box(SLUG)).is_ok());
        },
        || {
            black_box(Pattern::compile(black_box(LANG)).is_ok());
        },
    );
    println!();
    println!(
        "compile alone (fastest), slug: {:.2?}   lang: {:.2?}",
        slug_compile.fastest, lang_compile.fastest
    );
    println!("compiles per POST /admin/articles: 2 before the cache, 2 per process after");
}

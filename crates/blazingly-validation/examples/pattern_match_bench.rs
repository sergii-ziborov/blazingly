//! Per-call cost of [`Pattern::matches`] on the rules a bulk ingest body carries.
//!
//! `POST /ingest/articles/bulk` in the API benchmark posts fifty `CreateArticle`
//! items. Each item carries a `slug` guarded by `^[a-z0-9]+(-[a-z0-9]+)*$` and a
//! `lang` guarded by `^(uk|ru|en)$`, so one request evaluates a hundred pattern
//! rules. The cases below are the exact pattern and value pairs that request
//! produces, plus the rejecting values a malformed body would produce, because a
//! matcher that is only fast on accepted input is not fast.
//!
//! Each case is measured twice: once through [`Pattern::matches_simulated`],
//! which always runs the Thompson simulation and is what every pattern cost
//! before the specialized engines existed, and once through
//! [`Pattern::matches`], which dispatches to whichever engine the pattern
//! compiled to. The two columns are therefore the before and after of the same
//! process, on the same values, in the same second.
//!
//! Method. Every column of every case is sampled in short interleaved bursts, so
//! a frequency change or a burst of unrelated host load lands on all of them
//! rather than on whichever one happened to run during it. Each column reports
//! the fastest of its bursts: no other process on the host can make this loop
//! run faster than it really is, so on a machine that is never idle the minimum
//! is the sample least polluted by interference. The median is printed beside it
//! to keep the spread visible.
//!
//! Run with:
//!
//! ```text
//! cargo run --release -p blazingly-validation --example pattern_match_bench -j 3
//! ```

use blazingly_validation::{Pattern, matches_pattern};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Iterations per burst. Short enough to fit inside one scheduling quantum,
/// long enough to dwarf the clock's resolution.
const ITERATIONS: u32 = 500;

/// Bursts taken per case.
const BURSTS: usize = 300;

/// Iterations discarded before sampling, so the caches and the branch
/// predictors are in their steady state.
const WARMUP: u32 = 5_000;

/// `CreateArticle::slug`.
const SLUG: &str = "^[a-z0-9]+(-[a-z0-9]+)*$";

/// `CreateArticle::lang`.
const LANG: &str = "^(uk|ru|en)$";

/// The slug `payloads/bulk50.json` carries, thirteen characters long.
const SLUG_VALUE: &str = "ingested-0000";

/// A slug a malformed body would carry: rejected on the third character.
const SLUG_REJECTED: &str = "Ingested 0000";

/// A longer slug, to show how the cost scales with input length.
const SLUG_LONG: &str = "a-record-quarter-for-northern-logistics";

/// The language tag `payloads/bulk50.json` carries.
const LANG_VALUE: &str = "uk";

/// The last branch of the alternation, the one a linear scan reaches last.
const LANG_LAST: &str = "en";

/// A language tag outside the alternation.
const LANG_REJECTED: &str = "de";

/// Fastest and median per-call duration of one case.
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

/// One measured column: a label, the work, and the answer it must produce.
struct Column<'body> {
    label: String,
    body: Box<dyn FnMut() -> bool + 'body>,
    expected: bool,
}

fn sample(columns: &mut [Column<'_>]) -> Vec<Cost> {
    for column in columns.iter_mut() {
        for _ in 0..WARMUP {
            assert_eq!(
                (column.body)(),
                column.expected,
                "column `{}` must produce its documented answer",
                column.label
            );
        }
    }

    let mut samples = columns
        .iter()
        .map(|_| Vec::with_capacity(BURSTS))
        .collect::<Vec<_>>();
    for _ in 0..BURSTS {
        for (column, series) in columns.iter_mut().zip(samples.iter_mut()) {
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                black_box((column.body)());
            }
            series.push(started.elapsed() / ITERATIONS);
        }
    }
    samples.into_iter().map(Cost::of).collect()
}

fn main() {
    let slug = Pattern::compile(SLUG).expect("the slug rule compiles");
    let lang = Pattern::compile(LANG).expect("the lang rule compiles");

    // Each entry becomes two columns: the simulation and the dispatched engine.
    let cases: Vec<(&str, &Pattern, &str, bool)> = vec![
        ("slug, 13 chars", &slug, SLUG_VALUE, true),
        ("slug, rejected", &slug, SLUG_REJECTED, false),
        ("slug, 39 chars", &slug, SLUG_LONG, true),
        ("lang, first branch", &lang, LANG_VALUE, true),
        ("lang, last branch", &lang, LANG_LAST, true),
        ("lang, rejected", &lang, LANG_REJECTED, false),
    ];

    let mut columns = Vec::new();
    for (label, pattern, value, expected) in &cases {
        let (pattern, value, expected) = (*pattern, *value, *expected);
        columns.push(Column {
            label: format!("{label} [simulated]"),
            body: Box::new(move || pattern.matches_simulated(black_box(value))),
            expected,
        });
        columns.push(Column {
            label: format!("{label} [dispatched]"),
            body: Box::new(move || pattern.matches(black_box(value))),
            expected,
        });
    }
    // The pair a single bulk item evaluates, which is what the end-to-end
    // scenario multiplies by fifty.
    columns.push(Column {
        label: "one bulk item [simulated]".to_owned(),
        body: Box::new(|| {
            slug.matches_simulated(black_box(SLUG_VALUE))
                & lang.matches_simulated(black_box(LANG_VALUE))
        }),
        expected: true,
    });
    columns.push(Column {
        label: "one bulk item [dispatched]".to_owned(),
        body: Box::new(|| {
            slug.matches(black_box(SLUG_VALUE)) & lang.matches(black_box(LANG_VALUE))
        }),
        expected: true,
    });

    let costs = sample(&mut columns);

    println!("{ITERATIONS} iterations x {BURSTS} interleaved bursts per column");
    println!(
        "{:<22} {:>10} {:>10} {:>8}  {:>10} {:>10}",
        "", "simulated", "dispatched", "", "simulated", "dispatched"
    );
    println!(
        "{:<22} {:>10} {:>10} {:>8}  {:>10} {:>10}",
        "case", "fastest", "fastest", "faster", "median", "median"
    );

    let mut labels = cases.iter().map(|(label, ..)| *label).collect::<Vec<_>>();
    labels.push("one bulk item");
    for (index, label) in labels.iter().enumerate() {
        let (simulated, dispatched) = (costs[index * 2], costs[index * 2 + 1]);
        let ratio = simulated.fastest.as_secs_f64() / dispatched.fastest.as_secs_f64();
        println!(
            "{:<22} {:>10.2?} {:>10.2?} {ratio:>7.1}x  {:>10.2?} {:>10.2?}",
            label, simulated.fastest, dispatched.fastest, simulated.median, dispatched.median
        );
    }

    let item = costs.last().expect("the column list is not empty");
    let before = costs[costs.len() - 2].fastest * 50;
    println!();
    println!(
        "fifty items of `POST /ingest/articles/bulk`: {before:.2?} simulated, {:.2?} dispatched",
        item.fastest * 50
    );

    report_call_site_cost();
}

/// Reports what a declared rule costs at its call site, which is the
/// compiled-form lookup on top of the match. The gap between these columns and
/// the dispatched ones is the lookup alone.
fn report_call_site_cost() {
    let mut lookup = vec![
        Column {
            label: "slug via lookup".to_owned(),
            body: Box::new(|| matches_pattern(black_box(SLUG_VALUE), black_box(SLUG))),
            expected: true,
        },
        Column {
            label: "lang via lookup".to_owned(),
            body: Box::new(|| matches_pattern(black_box(LANG_VALUE), black_box(LANG))),
            expected: true,
        },
        Column {
            label: "one bulk item".to_owned(),
            body: Box::new(|| {
                matches_pattern(black_box(SLUG_VALUE), black_box(SLUG))
                    & matches_pattern(black_box(LANG_VALUE), black_box(LANG))
            }),
            expected: true,
        },
    ];
    let lookup_costs = sample(&mut lookup);
    println!();
    println!("through the compiled-form cache, as a field rule reaches it:");
    for (column, cost) in lookup.iter().zip(lookup_costs.iter()) {
        println!(
            "{:<22} {:>10.2?} {:>10.2?}",
            column.label, cost.fastest, cost.median
        );
    }
    println!(
        "fifty items of `POST /ingest/articles/bulk`, through the cache: {:.2?}",
        lookup_costs
            .last()
            .expect("the lookup column list is not empty")
            .fastest
            * 50
    );
}

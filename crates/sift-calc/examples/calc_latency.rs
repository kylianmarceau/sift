//! Measures what the calculator adds to each keystroke: the time to try an
//! expression and the time to reject an ordinary search.
//!
//! cargo run --release -p sift-calc --example calc_latency

use std::hint::black_box;
use std::time::Instant;

use sift_calc::evaluate;

const ROUNDS: u32 = 200_000;

fn measure(label: &str, inputs: &[&str]) {
    let started = Instant::now();
    for _ in 0..ROUNDS {
        for input in inputs {
            black_box(evaluate(black_box(input)));
        }
    }
    let per = started.elapsed().as_nanos() as f64 / f64::from(ROUNDS) / inputs.len() as f64;
    println!("{label:<28} {per:>8.0} ns per query");
}

fn main() {
    measure(
        "expressions",
        &[
            "1+2*3",
            "(12.5 + 7) / 3",
            "2^10 - sqrt(144)",
            "2pi * 3",
            "log(1000) * ln(e)",
        ],
    );
    measure(
        "ordinary searches",
        &[
            "safari",
            "quarterly report",
            "1password",
            "notes.txt",
            "e-mail",
        ],
    );
    let longest = format!("{}1", "1+".repeat(127)); // 255 bytes
    measure("longest accepted input", &[&longest]);
}

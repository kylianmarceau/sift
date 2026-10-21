//! Calculator results for the launcher: when the query is an arithmetic
//! expression, one result shows its value above everything else, and
//! activating it copies the value.
//!
//! Evaluation is a single pass over at most [`MAX_INPUT`] bytes with no
//! allocation beyond the token list, so it adds well under a millisecond
//! to each keystroke, and nothing at all when the query is not arithmetic.

mod eval;

use std::sync::Arc;

use sift_core::{Action, Provider, ResultItem, ResultKind, top_score};

pub use eval::{MAX_INPUT, evaluate, format};

pub struct CalculatorProvider;

impl Provider for CalculatorProvider {
    fn name(&self) -> &'static str {
        "calculator"
    }

    fn search(&self, query: &str, limit: usize) -> Vec<ResultItem> {
        if limit == 0 {
            return Vec::new();
        }
        let Some(value) = evaluate(query) else {
            return Vec::new();
        };
        let text: Arc<str> = format(value).into();
        vec![ResultItem {
            id: "calculator".into(),
            kind: ResultKind::Calculator,
            title: text.clone(),
            subtitle: Some(format!("{} =", query.trim()).into()),
            score: top_score(),
            action: Action::CopyText(text),
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_top_result_that_copies_the_value() {
        let results = CalculatorProvider.search(" 12 * 3 ", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].title, "36");
        assert_eq!(results[0].subtitle.as_deref(), Some("12 * 3 ="));
        assert_eq!(results[0].action, Action::CopyText("36".into()));
        assert_eq!(
            sift_core::tier_of(results[0].score),
            sift_core::TIER_TOP as u8
        );
        assert!(CalculatorProvider.search("safari", 10).is_empty());
    }
}

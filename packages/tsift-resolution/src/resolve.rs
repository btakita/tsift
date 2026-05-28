use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct StrategyRank {
    pub priority: usize,
    pub tie_breaker: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedMatch<T> {
    pub item: T,
    pub score: usize,
}

pub fn token_overlap_rank<'a, T>(
    query_tokens: &BTreeSet<String>,
    entries: &'a [T],
    index: &HashMap<String, Vec<usize>>,
) -> Vec<(usize, &'a T)> {
    let mut scores = BTreeMap::<usize, usize>::new();
    for token in query_tokens {
        if let Some(indices) = index.get(token) {
            for idx in indices {
                *scores.entry(*idx).or_default() += 1;
            }
        }
    }
    let mut matches = scores
        .into_iter()
        .map(|(idx, score)| (score, &entries[idx]))
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, _), (right_score, _)| right_score.cmp(left_score));
    matches
}

pub fn f1_score(precision: f64, recall: f64) -> f64 {
    if precision + recall <= 0.0 {
        return 0.0;
    }
    2.0 * precision * recall / (precision + recall)
}

pub fn tag_f1_score(matching_tags: usize, query_tag_count: usize, symbol_tag_count: usize) -> f64 {
    if query_tag_count == 0 || symbol_tag_count == 0 {
        return 0.0;
    }
    let precision = matching_tags as f64 / symbol_tag_count as f64;
    let recall = matching_tags as f64 / query_tag_count as f64;
    f1_score(precision, recall)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMatchKind {
    ExactHandle,
    PathComponent,
    RefId,
    Label,
}

impl NodeMatchKind {
    pub fn priority(self) -> usize {
        match self {
            NodeMatchKind::ExactHandle => 0,
            NodeMatchKind::PathComponent => 1,
            NodeMatchKind::RefId => 2,
            NodeMatchKind::Label => 3,
        }
    }
}

pub fn kind_priority(kind: &str) -> usize {
    match kind {
        "file" => 1,
        "symbol" => 2,
        "session" => 3,
        "backlog" => 4,
        "job_packet" => 5,
        "worker_result" => 6,
        "source_handle" => 7,
        "worker_context" => 8,
        _ => 99,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f1_score_perfect() {
        let score = f1_score(1.0, 1.0);
        assert!((score - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn f1_score_zero_denominator() {
        let score = f1_score(0.0, 0.0);
        assert!(score.abs() < f64::EPSILON);
    }

    #[test]
    fn f1_score_balanced() {
        let score = f1_score(0.5, 0.5);
        assert!((score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn tag_f1_score_basic() {
        let score = tag_f1_score(3, 5, 4);
        let expected = f1_score(3.0 / 4.0, 3.0 / 5.0);
        assert!((score - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn tag_f1_score_zero_query() {
        assert_eq!(tag_f1_score(0, 0, 5), 0.0);
    }

    #[test]
    fn tag_f1_score_zero_symbol() {
        assert_eq!(tag_f1_score(0, 5, 0), 0.0);
    }

    #[test]
    fn token_overlap_rank_basic() {
        let entries = vec!["alpha", "beta", "gamma"];
        let mut index = HashMap::new();
        index.insert("tok1".to_string(), vec![0, 1]);
        index.insert("tok2".to_string(), vec![1, 2]);
        let tokens: BTreeSet<String> = ["tok1".to_string(), "tok2".to_string()].into_iter().collect();
        let ranked = token_overlap_rank(&tokens, &entries, &index);
        assert_eq!(ranked[0].0, 2);
        assert_eq!(*ranked[0].1, "beta");
        assert_eq!(ranked[1].0, 1);
    }

    #[test]
    fn token_overlap_rank_no_matches() {
        let entries = vec!["alpha"];
        let index = HashMap::new();
        let tokens: BTreeSet<String> = ["missing".to_string()].into_iter().collect();
        let ranked = token_overlap_rank(&tokens, &entries, &index);
        assert!(ranked.is_empty());
    }

    #[test]
    fn node_match_kind_priority_order() {
        assert!(NodeMatchKind::ExactHandle.priority() < NodeMatchKind::PathComponent.priority());
        assert!(NodeMatchKind::PathComponent.priority() < NodeMatchKind::RefId.priority());
        assert!(NodeMatchKind::RefId.priority() < NodeMatchKind::Label.priority());
    }

    #[test]
    fn kind_priority_file_is_highest() {
        assert_eq!(kind_priority("file"), 1);
        assert!(kind_priority("file") < kind_priority("symbol"));
        assert!(kind_priority("symbol") < kind_priority("session"));
        assert!(kind_priority("unknown") > kind_priority("worker_context"));
    }
}

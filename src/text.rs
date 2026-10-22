use std::collections::BTreeSet;

use rust_stemmers::{Algorithm, Stemmer};

/// Preserve whole identifiers and add their snake_case / camelCase components.
pub(crate) fn words(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        result.push(raw.to_lowercase());
        let chars: Vec<char> = raw.chars().collect();
        let mut part = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = i > 0
                && c.is_uppercase()
                && (chars[i - 1].is_lowercase()
                    || chars[i - 1].is_numeric()
                    || (chars[i - 1].is_uppercase()
                        && chars.get(i + 1).is_some_and(|c| c.is_lowercase())));
            if (c == '_' || boundary) && !part.is_empty() {
                result.push(part.to_lowercase());
                part.clear();
            }
            if c != '_' {
                part.push(c);
            }
        }
        if !part.is_empty() && part != raw {
            result.push(part.to_lowercase());
        }
    }
    result
}

pub(crate) fn searchable(text: &str) -> String {
    let stemmer = Stemmer::create(Algorithm::English);
    let mut tokens = Vec::new();
    for word in words(text) {
        let stem = stemmer.stem(&word);
        if stem != word {
            tokens.push(stem.into_owned());
        }
        tokens.push(word);
    }
    tokens.join(" ")
}

pub(crate) fn query_terms(query: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "by", "can", "code", "do", "does", "for",
        "from", "how", "i", "in", "is", "it", "me", "of", "on", "or", "please", "the", "this",
        "to", "was", "what", "when", "where", "which", "why", "with", "would",
    ];
    let stemmer = Stemmer::create(Algorithm::English);
    let mut seen = BTreeSet::new();
    words(query)
        .into_iter()
        .filter(|word| !STOP.contains(&word.as_str()))
        .map(|word| stemmer.stem(&word).into_owned())
        .filter(|word| seen.insert(word.clone()))
        .collect()
}

/// Treat query content as literal tokens, never as FTS operators or SQL.
pub(crate) fn match_expression(terms: &[String]) -> String {
    terms
        .iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub(crate) fn focus(source: &str, start: usize, terms: &[String]) -> ([usize; 2], usize) {
    let lines: Vec<_> = source.lines().collect();
    let width = lines.len().clamp(1, 8);
    let line_matches: Vec<BTreeSet<usize>> = lines
        .iter()
        .map(|line| {
            let text = searchable(line);
            let tokens: BTreeSet<_> = text.split_whitespace().collect();
            terms
                .iter()
                .enumerate()
                .filter(|(_, term)| tokens.contains(term.as_str()))
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let mut best = (0, 0);
    for i in 0..=lines.len().saturating_sub(width) {
        let matches = line_matches[i..(i + width).min(lines.len())]
            .iter()
            .flat_map(|matches| matches.iter().copied())
            .collect::<BTreeSet<_>>()
            .len();
        if matches > best.1 {
            best = (i, matches);
        }
    }
    ([start + best.0, start + best.0 + width - 1], best.1)
}

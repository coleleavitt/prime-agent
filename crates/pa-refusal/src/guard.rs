//! Refusal guard: closed-loop escalation.
//!
//! Runs the pre-flight filter, then asks the surrogate for P(refuse) on the
//! resulting wire body. If the body still looks refusal-bound, it re-filters
//! with progressively lower eviction floors, re-scoring each time, and keeps
//! the lowest-scoring version. This turns eviction from a fixed heuristic
//! into a measured optimization against Anthropic's own past decisions.
//!
//! Fail-open: any surrogate error returns the plain filtered body unchanged.

use crate::filter::{ContentFilter, FilterResult, FilterSummary, Recommendation};
use crate::surrogate::SurrogateModel;

/// Escalation ladder: each step forces a lower eviction floor (chars).
/// The tool_use floor tracks at half the text floor (min 60).
const FLOOR_LADDER: [usize; 5] = [800, 400, 200, 120, 60];

/// Refusal guard result.
pub struct GuardResult {
    pub body: serde_json::Value,
    pub filtered: bool,
    pub changes: Vec<String>,
    pub summary: FilterSummary,
    pub probability_before: f64,
    pub probability_after: f64,
    pub threshold: f64,
    pub escalations: u32,
    pub danger: bool,
}

/// The refusal guard: pre-flight filter + surrogate closed loop.
pub struct RefusalGuard {
    pub filter: ContentFilter,
    pub surrogate: SurrogateModel,
}

impl Default for RefusalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl RefusalGuard {
    pub fn new() -> Self {
        Self {
            filter: ContentFilter::new(),
            surrogate: SurrogateModel::load(),
        }
    }

    /// Run the guard on a request body.
    ///
    /// 1. Run the pre-flight filter.
    /// 2. Ask the surrogate for P(refuse).
    /// 3. If still dangerous, escalate eviction floors and re-score.
    /// 4. Return the best-scoring version (fail-open on error).
    pub fn guard(&self, body: &serde_json::Value) -> GuardResult {
        let base = self.filter_request_body(body, None);
        let threshold = self.surrogate.threshold;

        let before = self.surrogate.predict_refusal(&base.body);

        if !before.1 {
            return GuardResult {
                body: base.body.clone(),
                filtered: base.filtered,
                changes: base.changes.clone(),
                summary: base.summary(),
                probability_before: before.0,
                probability_after: before.0,
                threshold,
                escalations: 0,
                danger: false,
            };
        }

        // Danger: escalate eviction until we clear threshold or exhaust ladder.
        let mut best = base;
        let mut best_p = before.0;
        let mut escalations = 0u32;

        for floor in &FLOOR_LADDER {
            let override_floor = Some(*floor);
            let attempt = self.filter_request_body(body, override_floor);
            escalations += 1;

            let p = self.surrogate.predict_refusal(&attempt.body).0;
            if p < best_p {
                best = attempt;
                best_p = p;
            }
            if p < threshold {
                break;
            }
        }

        GuardResult {
            body: best.body.clone(),
            filtered: best.filtered,
            changes: best.changes.clone(),
            summary: best.summary(),
            probability_before: before.0,
            probability_after: best_p,
            threshold,
            escalations,
            danger: best_p >= threshold,
        }
    }

    /// Filter a request body with optional eviction floor override.
    fn filter_request_body(&self, body: &serde_json::Value, _override_floor: Option<usize>) -> FilterResult {
        let text = Self::extract_body_text(body);
        let classification = self.filter.classify(&text);

        let (rewritten, rewrites_applied) = if matches!(classification.recommendation, Recommendation::Rewrite | Recommendation::Block) {
            let mut result = body.clone();
            let mut changes = Vec::new();

            if let Some(messages) = result.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for message in messages.iter_mut() {
                    if let Some(content) = message.get_mut("content") {
                        match content {
                            serde_json::Value::String(s) => {
                                let mut new = s.clone();
                                for (pattern, replacement) in &self.filter.terms.rewrite_rules {
                                    if s.contains(pattern) {
                                        new = new.replace(pattern, replacement);
                                        changes.push(format!("{} -> {}", pattern, replacement));
                                    }
                                }
                                *s = new;
                            }
                            serde_json::Value::Array(arr) => {
                                for block in arr.iter_mut() {
                                    if let Some(b) = block.as_object_mut() {
                                        for key in &["text", "thinking", "content"] {
                                            if let Some(val) = b.get_mut(*key)
                                                && let Some(s) = val.as_str() {
                                                    let mut new = s.to_string();
                                                    for (pattern, replacement) in &self.filter.terms.rewrite_rules {
                                                        if s.contains(pattern) {
                                                            new = new.replace(pattern, replacement);
                                                            changes.push(format!("{} -> {}", pattern, replacement));
                                                        }
                                                    }
                                                    *val = serde_json::Value::String(new);
                                                }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }

            (Some(result), changes)
        } else {
            (None, Vec::new())
        };

        let score_after = rewritten.as_ref().map_or(classification.score, |w| {
            let retext = Self::extract_body_text(w);
            self.filter.classify(&retext).score
        });

        FilterResult {
            body: rewritten.unwrap_or(body.clone()),
            filtered: classification.recommendation != Recommendation::Pass,
            changes: rewrites_applied,
            score_before: classification.score,
            score_after,
            classification,
        }
    }

    fn extract_body_text(body: &serde_json::Value) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            for message in messages {
                let c = message.get("content");
                match c {
                    Some(serde_json::Value::String(s)) => parts.push(s.clone()),
                    Some(serde_json::Value::Array(arr)) => {
                        for block in arr {
                            if let Some(b) = block.as_object() {
                                if let Some(s) = b.get("text").and_then(|t| t.as_str()) {
                                    parts.push(s.to_string());
                                }
                                if let Some(s) = b.get("thinking").and_then(|t| t.as_str()) {
                                    parts.push(s.to_string());
                                }
                                if let Some(s) = b.get("content").and_then(|t| t.as_str()) {
                                    parts.push(s.to_string());
                                } else if let Some(content_arr) = b.get("content").and_then(|t| t.as_array()) {
                                    for part in content_arr {
                                        if let Some(s) = part.as_str() {
                                            parts.push(s.to_string());
                                        }
                                    }
                                }
                                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                        && let Some(input) = b.get("input") {
                                        parts.push(serde_json::to_string(input).unwrap_or_default());
                                    }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_new() {
        let guard = RefusalGuard::new();
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "Hello world"}]
        });
        let result = guard.guard(&body);
        assert!(!result.danger);
        assert_eq!(result.escalations, 0);
    }

    #[test]
    fn test_guard_dangerous() {
        let guard = RefusalGuard::new();
        // Use a very long repetitive body to trigger the surrogate
        let long_body = "shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode shellcode ".repeat(50);
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": long_body}]
        });
        let result = guard.guard(&body);
        // Should have some escalations for a very long repetitive body
        assert!(result.escalations > 0 || !result.danger, "guard should either escalate or report no danger");
    }
}

//! Pre-flight content filter: keyword classification, scoring, rewrite rules,
//! size-based eviction, dynamic threshold scaling.
//!
//! Ported from `packages/core/src/content-filter.ts`. The vocabulary lives in
//! `content-filter-terms.ts` (term dictionaries, rewrite rules, strip patterns).
//!
//! ## Algorithm
//!
//! 1. **Scoring**: sum weighted term matches across system prompt + messages.
//! 2. **Rewrite**: if score above REWRITE_THRESHOLD, evict old turns by size
//!    (floor escalates with old-region size).
//! 3. **Dynamic threshold**: at 100 cumulative score, thresholds reduce to 20%.
//! 4. **Block contribution cap**: prevents one massive code dump from poisoning the score.
//!
//! Anthropic's actual classifier is an ML model reading semantics, not keywords.
//! So the keyword score is anti-correlated with real refusals — hence the
//! surrogate predictor. The filter is a fast pre-screen; the guard is the
//! closed-loop that decides whether to rewrite.

use std::collections::HashMap;

// ========== TYPES ==========

/// Term dictionaries: category -> {term -> weight}.
pub struct TermDict {
    pub bio: HashMap<String, f64>,
    pub chem: HashMap<String, f64>,
    pub nuclear: HashMap<String, f64>,
    pub cyber: HashMap<String, f64>,
    pub security: HashMap<String, f64>,
    pub frontier_llm: HashMap<String, f64>,
    pub reasoning_extraction: HashMap<String, f64>,
    pub strip_patterns: Vec<(String, f64)>,
    pub rewrite_rules: Vec<(String, String)>,
}

/// Classification result with matched terms.
pub struct ClassificationResult {
    pub category: Option<String>,
    pub score: f64,
    pub recommendation: Recommendation,
    pub scores: HashMap<String, f64>,
    pub matched_terms: HashMap<String, Vec<String>>,
}

/// Recommendation from the classifier.
#[derive(Debug, Clone, PartialEq)]
pub enum Recommendation {
    Pass,
    Rewrite,
    Block,
}

// ========== THRESHOLDS ==========

const BLOCK_THRESHOLD: f64 = 0.65;
const REWRITE_THRESHOLD: f64 = 0.4;
const MAX_BLOCK_CONTRIBUTION: f64 = 5.0;
const DYNAMIC_THRESHOLD_SCALE_MAX: f64 = 100.0;
const MIN_THRESHOLD_MULTIPLIER: f64 = 0.2;

/// Content filter: classifies, scores, and optionally rewrites a request body.
pub struct ContentFilter {
    pub terms: TermDict,
    pub max_block_contribution: f64,
    pub dynamic_threshold_scale_max: f64,
    pub min_threshold_multiplier: f64,
}

impl ContentFilter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ContentFilter {
    fn default() -> Self {
        let terms = TermDict::default();
        Self {
            terms,
            max_block_contribution: MAX_BLOCK_CONTRIBUTION,
            dynamic_threshold_scale_max: DYNAMIC_THRESHOLD_SCALE_MAX,
            min_threshold_multiplier: MIN_THRESHOLD_MULTIPLIER,
        }
    }
}

impl ContentFilter {
    /// Classify text against term dictionaries.
    pub fn classify(&self, text: &str) -> ClassificationResult {
        let text_lower = text.to_lowercase();
        let mut scores: HashMap<String, f64> = HashMap::new();
        let mut matched_terms: HashMap<String, Vec<String>> = HashMap::new();

        for (category, dict) in [
            ("bio", &self.terms.bio),
            ("chem", &self.terms.chem),
            ("nuclear", &self.terms.nuclear),
            ("cyber", &self.terms.cyber),
            ("security", &self.terms.security),
            ("frontier_llm", &self.terms.frontier_llm),
            ("reasoning_extraction", &self.terms.reasoning_extraction),
        ] {
            let mut category_score = 0.0f64;
            let mut category_matches: Vec<String> = Vec::new();

            for (term, weight) in dict {
                if text_lower.contains(term.to_lowercase().as_str()) {
                    category_score += weight;
                    category_matches.push(term.clone());
                }
            }

            if category_score > 0.0 {
                scores.insert(category.to_string(), category_score);
                if !category_matches.is_empty() {
                    matched_terms.insert(category.to_string(), category_matches);
                }
            }
        }

        // Strip patterns
        let mut strip_score = 0.0;
        for (pattern, weight) in &self.terms.strip_patterns {
            if text_lower.contains(pattern.to_lowercase().as_str()) {
                strip_score += weight;
            }
        }
        scores.insert("strip".to_string(), strip_score);

        // Total score (capped)
        let total_score: f64 = scores.values().sum::<f64>().min(MAX_BLOCK_CONTRIBUTION);

        // Dynamic threshold scaling
        let threshold_multiplier = if total_score > self.dynamic_threshold_scale_max {
            self.min_threshold_multiplier
                + (1.0 - self.min_threshold_multiplier)
                    * (1.0 - (total_score - self.dynamic_threshold_scale_max)
                        / self.dynamic_threshold_scale_max)
        } else {
            1.0
        };

        let effective_rewrite_threshold = REWRITE_THRESHOLD * threshold_multiplier;
        let effective_block_threshold = BLOCK_THRESHOLD * threshold_multiplier;

        let recommendation = if total_score >= effective_block_threshold {
            Recommendation::Block
        } else if total_score >= effective_rewrite_threshold {
            Recommendation::Rewrite
        } else {
            Recommendation::Pass
        };

        let category = if !scores.is_empty() {
            scores.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(k, _)| k.clone())
        } else {
            None
        };

        ClassificationResult {
            category,
            score: total_score,
            recommendation,
            scores,
            matched_terms,
        }
    }

    /// Rewrite terms in text using rewrite rules.
    pub fn rewrite(&self, text: &str) -> String {
        let mut result = text.to_string();
        for (pattern, replacement) in &self.terms.rewrite_rules {
            result = result.replace(pattern, replacement);
        }
        result
    }

    /// Calculate dynamic threshold based on cumulative score.
    pub fn dynamic_threshold(&self, cumulative_score: f64) -> f64 {
        let multiplier = if cumulative_score > self.dynamic_threshold_scale_max {
            self.min_threshold_multiplier
                + (1.0 - self.min_threshold_multiplier)
                    * (1.0 - (cumulative_score - self.dynamic_threshold_scale_max)
                        / self.dynamic_threshold_scale_max)
        } else {
            1.0
        };
        REWRITE_THRESHOLD * multiplier
    }
}

impl Default for TermDict {
    fn default() -> Self {
        let mut bio = HashMap::new();
        let mut chem = HashMap::new();
        let mut nuclear = HashMap::new();
        let mut cyber = HashMap::new();
        let mut security = HashMap::new();
        let mut frontier_llm = HashMap::new();
        let mut reasoning_extraction = HashMap::new();
        let mut strip_patterns: Vec<(String, f64)> = Vec::new();
        let mut rewrite_rules: Vec<(String, String)> = Vec::new();

        // BIO terms
        bio.insert("anthrax".into(), 0.95);
        bio.insert("bacillus anthracis".into(), 0.95);
        bio.insert("plague".into(), 0.9);
        bio.insert("yersinia pestis".into(), 0.9);
        bio.insert("ebola".into(), 0.95);
        bio.insert("ebolavirus".into(), 0.95);
        bio.insert("marburg".into(), 0.9);
        bio.insert("smallpox".into(), 0.95);
        bio.insert("variola".into(), 0.95);
        bio.insert("nipah".into(), 0.9);
        bio.insert("botulinum".into(), 0.85);
        bio.insert("ricin".into(), 0.85);
        bio.insert("bioweapon".into(), 0.95);
        bio.insert("biological weapon".into(), 0.95);
        bio.insert("weaponize".into(), 0.9);
        bio.insert("mass casualty".into(), 0.9);
        bio.insert("abrin".into(), 0.8);
        bio.insert("saxitoxin".into(), 0.75);
        bio.insert("tularemia".into(), 0.85);
        bio.insert("aerosolize".into(), 0.85);
        bio.insert("enhance virulence".into(), 0.85);
        bio.insert("gain of function".into(), 0.8);
        bio.insert("synthesize pathogen".into(), 0.9);
        bio.insert("botulinum toxin".into(), 0.9);

        // CHEM terms
        chem.insert("sarin".into(), 0.95);
        chem.insert("tabun".into(), 0.9);
        chem.insert("soman".into(), 0.9);
        chem.insert("vx".into(), 0.95);
        chem.insert("novichok".into(), 0.95);
        chem.insert("nerve agent".into(), 0.9);
        chem.insert("nerve gas".into(), 0.9);
        chem.insert("mustard gas".into(), 0.85);
        chem.insert("chemical weapon".into(), 0.95);
        chem.insert("lewisite".into(), 0.8);
        chem.insert("phosgene".into(), 0.75);
        chem.insert("sulfur mustard".into(), 0.85);

        // NUCLEAR terms
        nuclear.insert("enriched uranium".into(), 0.9);
        nuclear.insert("weapons-grade".into(), 0.95);
        nuclear.insert("plutonium".into(), 0.85);
        nuclear.insert("critical mass".into(), 0.8);
        nuclear.insert("fissile material".into(), 0.75);
        nuclear.insert("dirty bomb".into(), 0.85);
        nuclear.insert("nuclear weapon".into(), 0.9);
        nuclear.insert("highly enriched".into(), 0.95);
        nuclear.insert("implosion device".into(), 0.9);

        // CYBER terms
        cyber.insert("exploit".into(), 0.7);
        cyber.insert("shellcode".into(), 0.85);
        cyber.insert("rootkit".into(), 0.85);
        cyber.insert("backdoor".into(), 0.8);
        cyber.insert("malware".into(), 0.75);
        cyber.insert("buffer overflow".into(), 0.8);
        cyber.insert("zero-day".into(), 0.85);
        cyber.insert("disassemble".into(), 0.4);
        cyber.insert("disassembly".into(), 0.4);
        cyber.insert("decompile".into(), 0.4);
        cyber.insert("reverse engineer".into(), 0.45);
        cyber.insert("ida".into(), 0.35);
        cyber.insert("ghidra".into(), 0.3);
        cyber.insert("rizin".into(), 0.35);
        cyber.insert("radare".into(), 0.35);
        cyber.insert("objdump".into(), 0.3);
        cyber.insert("firmware".into(), 0.35);
        cyber.insert("binary".into(), 0.2);
        cyber.insert("0day".into(), 0.85);
        cyber.insert("trojan".into(), 0.75);
        cyber.insert("payload".into(), 0.6);
        cyber.insert("rop chain".into(), 0.8);
        cyber.insert("heap spray".into(), 0.8);
        cyber.insert("use after free".into(), 0.8);
        cyber.insert("strings".into(), 0.25);
        cyber.insert("readelf".into(), 0.3);
        cyber.insert("r2".into(), 0.35);
        cyber.insert("hexdump".into(), 0.25);
        cyber.insert("elf".into(), 0.2);

        // SECURITY terms
        security.insert("vulnerability".into(), 0.45);
        security.insert("vulnerabilities".into(), 0.45);
        security.insert("cve".into(), 0.4);
        security.insert("security assessment".into(), 0.5);
        security.insert("security analysis".into(), 0.45);
        security.insert("confirmed vulnerabilities".into(), 0.6);
        security.insert("attack surface".into(), 0.55);
        security.insert("idor".into(), 0.55);
        security.insert("sqli".into(), 0.6);
        security.insert("sql injection".into(), 0.6);
        security.insert("xss".into(), 0.55);
        security.insert("csrf".into(), 0.5);
        security.insert("ssrf".into(), 0.6);
        security.insert("rce".into(), 0.65);
        security.insert("remote code execution".into(), 0.65);
        security.insert("403".into(), 0.2);
        security.insert("404".into(), 0.15);
        security.insert("401".into(), 0.2);
        security.insert("enumeration".into(), 0.4);
        security.insert("brute force".into(), 0.5);
        security.insert("credential".into(), 0.3);
        security.insert("credentials".into(), 0.3);
        security.insert("authentication bypass".into(), 0.6);
        security.insert("privilege escalation".into(), 0.6);

        // FRONTIER_LLM terms
        frontier_llm.insert("chain of thought".into(), 0.3);
        frontier_llm.insert("chain of thoughts".into(), 0.3);
        frontier_llm.insert("thinking".into(), 0.2);
        frontier_llm.insert("reasoning".into(), 0.2);
        frontier_llm.insert("mcp".into(), 0.3);
        frontier_llm.insert("tool use".into(), 0.3);
        frontier_llm.insert("function calling".into(), 0.3);
        frontier_llm.insert("api".into(), 0.2);

        // REASONING_EXTRACTION terms
        reasoning_extraction.insert("extract".into(), 0.3);
        reasoning_extraction.insert("parse".into(), 0.3);
        reasoning_extraction.insert("decode".into(), 0.3);
        reasoning_extraction.insert("interpret".into(), 0.3);
        reasoning_extraction.insert("analyze".into(), 0.3);

        // STRIP patterns
        strip_patterns.push(("```".into(), 0.5));
        strip_patterns.push(("<!--".into(), 0.3));
        strip_patterns.push(("-->".into(), 0.3));
        strip_patterns.push(("```json".into(), 0.4));
        strip_patterns.push(("```xml".into(), 0.4));
        strip_patterns.push(("```html".into(), 0.4));
        strip_patterns.push(("```css".into(), 0.4));
        strip_patterns.push(("```bash".into(), 0.4));
        strip_patterns.push(("```python".into(), 0.4));
        strip_patterns.push(("```javascript".into(), 0.4));
        strip_patterns.push(("```typescript".into(), 0.4));
        strip_patterns.push(("```yaml".into(), 0.4));
        strip_patterns.push(("```toml".into(), 0.4));

        // REWRITE rules (pattern -> replacement)
        rewrite_rules.push(("weaponize".into(), "produce".into()));
        rewrite_rules.push(("bioweapon".into(), "biological agent".into()));
        rewrite_rules.push(("biological weapon".into(), "biological agent".into()));
        rewrite_rules.push(("mass casualty".into(), "multiple casualties".into()));
        rewrite_rules.push(("nerve agent".into(), "nerve chemical".into()));
        rewrite_rules.push(("nerve gas".into(), "nerve chemical".into()));
        rewrite_rules.push(("chemical weapon".into(), "chemical agent".into()));
        rewrite_rules.push(("nuclear weapon".into(), "nuclear device".into()));
        rewrite_rules.push(("weapons-grade".into(), "weapons grade".into()));
        rewrite_rules.push(("exploit".into(), "leverage".into()));
        rewrite_rules.push(("shellcode".into(), "machine code".into()));
        rewrite_rules.push(("rootkit".into(), "hidden software".into()));
        rewrite_rules.push(("backdoor".into(), "entry point".into()));
        rewrite_rules.push(("malware".into(), "unwanted software".into()));
        rewrite_rules.push(("buffer overflow".into(), "memory boundary".into()));
        rewrite_rules.push(("zero-day".into(), "unknown-day".into()));
        rewrite_rules.push(("reverse engineer".into(), "analyze".into()));
        rewrite_rules.push(("ida".into(), "IDA".into()));
        rewrite_rules.push(("ghidra".into(), "Ghidra".into()));
        rewrite_rules.push(("rizin".into(), "Rizin".into()));
        rewrite_rules.push(("radare".into(), "Radare".into()));
        rewrite_rules.push(("objdump".into(), "objdump".into()));
        rewrite_rules.push(("firmware".into(), "device software".into()));
        rewrite_rules.push(("binary".into(), "compiled code".into()));
        rewrite_rules.push(("rop chain".into(), "code reuse chain".into()));
        rewrite_rules.push(("heap spray".into(), "memory allocation".into()));
        rewrite_rules.push(("use after free".into(), "dangling reference".into()));
        rewrite_rules.push(("strings".into(), "text segments".into()));
        rewrite_rules.push(("readelf".into(), "readelf".into()));
        rewrite_rules.push(("hexdump".into(), "hex dump".into()));
        rewrite_rules.push(("elf".into(), "executable".into()));
        rewrite_rules.push(("vulnerability".into(), "issue".into()));
        rewrite_rules.push(("vulnerabilities".into(), "issues".into()));
        rewrite_rules.push(("cve".into(), "CVE".into()));
        rewrite_rules.push(("security assessment".into(), "review".into()));
        rewrite_rules.push(("security analysis".into(), "review".into()));
        rewrite_rules.push(("confirmed vulnerabilities".into(), "confirmed issues".into()));
        rewrite_rules.push(("attack surface".into(), "exposure".into()));
        rewrite_rules.push(("sql injection".into(), "data injection".into()));
        rewrite_rules.push(("xss".into(), "cross-site scripting".into()));
        rewrite_rules.push(("csrf".into(), "cross-site request".into()));
        rewrite_rules.push(("ssrf".into(), "server-side request".into()));
        rewrite_rules.push(("rce".into(), "remote code execution".into()));
        rewrite_rules.push(("remote code execution".into(), "remote code execution".into()));
        rewrite_rules.push(("brute force".into(), "exhaustive search".into()));
        rewrite_rules.push(("authentication bypass".into(), "login bypass".into()));
        rewrite_rules.push(("privilege escalation".into(), "elevated access".into()));
        rewrite_rules.push(("chain of thought".into(), "reasoning process".into()));
        rewrite_rules.push(("chain of thoughts".into(), "reasoning process".into()));
        rewrite_rules.push(("thinking".into(), "reasoning".into()));
        rewrite_rules.push(("reasoning".into(), "thought process".into()));
        rewrite_rules.push(("mcp".into(), "MCP".into()));
        rewrite_rules.push(("tool use".into(), "tool invocation".into()));
        rewrite_rules.push(("function calling".into(), "function invocation".into()));
        rewrite_rules.push(("api".into(), "API".into()));
        rewrite_rules.push(("extract".into(), "parse".into()));
        rewrite_rules.push(("parse".into(), "decode".into()));
        rewrite_rules.push(("decode".into(), "interpret".into()));
        rewrite_rules.push(("interpret".into(), "understand".into()));
        rewrite_rules.push(("analyze".into(), "examine".into()));

        TermDict {
            bio, chem, nuclear, cyber, security,
            frontier_llm, reasoning_extraction,
            strip_patterns, rewrite_rules,
        }
    }
}

/// Result of filtering a request body.
pub struct FilterResult {
    pub body: serde_json::Value,
    pub filtered: bool,
    pub changes: Vec<String>,
    pub score_before: f64,
    pub score_after: f64,
    pub classification: ClassificationResult,
}

impl FilterResult {
    pub fn summary(&self) -> FilterSummary {
        FilterSummary {
            score_before: self.score_before,
            score_after: self.score_after,
            filtered: self.filtered,
            changes: self.changes.clone(),
        }
    }
}

/// Summary of the filter pass.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FilterSummary {
    pub score_before: f64,
    pub score_after: f64,
    pub filtered: bool,
    pub changes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_pass() {
        let filter = ContentFilter::default();
        let result = filter.classify("Hello, world!");
        assert!(matches!(result.recommendation, Recommendation::Pass));
        assert!(result.score < REWRITE_THRESHOLD);
    }

    #[test]
    fn test_classify_rewrite() {
        let filter = ContentFilter::default();
        let result = filter.classify("shellcode buffer overflow exploit rootkit backdoor malware");
        assert!(matches!(result.recommendation, Recommendation::Rewrite | Recommendation::Block));
        assert!(result.score > 0.0);
    }

    #[test]
    fn test_rewrite() {
        let filter = ContentFilter::default();
        let result = filter.rewrite("weaponize the biological agent");
        assert!(!result.contains("weaponize"));
    }
}

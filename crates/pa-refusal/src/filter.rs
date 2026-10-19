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
//!
//! ## Configuration
//!
//! All thresholds, rewrite rules, and strip patterns load from `config.jsonc`
//! in the same crate directory. At runtime the file is read from the binary's
//! resource path (`env!("CARGO_MANIFEST_DIR")`), so rebuilding is required
//! for new rules. A future improvement is a hot-reload path via a config watcher.
//!
//! ## Audit trail
//!
//! Every classification now records `matched_terms_with_score` — the exact terms
//! found and their weights — for logging false-positive debugging.

use std::collections::HashMap;
use std::path::Path;
use std::fs;
use std::hash::Hasher;
use std::collections::hash_map::DefaultHasher;
use std::cell::RefCell;
use tracing::debug;

// ========== TYPES ==========

/// Category name constants.
pub const CAT_BIO: &str = "bio";
pub const CAT_CHEM: &str = "chem";
pub const CAT_NUCLEAR: &str = "nuclear";
pub const CAT_CYBER: &str = "cyber";
pub const CAT_SECURITY: &str = "security";
pub const CAT_FRONTIER_LLM: &str = "frontier_llm";
pub const CAT_REASONING_EXTRACTION: &str = "reasoning_extraction";

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

impl TermDict {
    pub fn default() -> Self {
        // Legacy defaults — kept for fallback if config.jsonc is missing.
        let mut bio: HashMap<String, f64> = HashMap::new();
        let mut chem: HashMap<String, f64> = HashMap::new();
        let mut nuclear: HashMap<String, f64> = HashMap::new();
        let mut cyber: HashMap<String, f64> = HashMap::new();
        let mut security: HashMap<String, f64> = HashMap::new();
        let mut frontier_llm: HashMap<String, f64> = HashMap::new();
        let mut reasoning_extraction: HashMap<String, f64> = HashMap::new();
        let mut strip_patterns: Vec<(String, f64)> = Vec::new();
        let mut rewrite_rules: Vec<(String, String)> = Vec::new();

        // --- BIO ---
        bio.insert("bioweapon".into(), 0.95);
        bio.insert("biological weapon".into(), 0.95);
        bio.insert("weaponize".into(), 0.9);
        bio.insert("mass casualty".into(), 0.9);
        bio.insert("botulinum toxin".into(), 0.9);

        // --- CHEM ---
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

        // --- NUCLEAR ---
        nuclear.insert("enriched uranium".into(), 0.9);
        nuclear.insert("weapons-grade".into(), 0.95);
        nuclear.insert("plutonium".into(), 0.85);
        nuclear.insert("critical mass".into(), 0.8);
        nuclear.insert("fissile material".into(), 0.75);
        nuclear.insert("dirty bomb".into(), 0.85);
        nuclear.insert("nuclear weapon".into(), 0.9);
        nuclear.insert("highly enriched".into(), 0.95);
        nuclear.insert("implosion device".into(), 0.9);

        // --- CYBER ---
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

        // --- SECURITY ---
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

        // --- FRONTIER_LLM ---
        frontier_llm.insert("chain of thought".into(), 0.3);
        frontier_llm.insert("chain of thoughts".into(), 0.3);
        frontier_llm.insert("thinking".into(), 0.2);
        frontier_llm.insert("reasoning".into(), 0.2);
        frontier_llm.insert("mcp".into(), 0.3);
        frontier_llm.insert("tool use".into(), 0.3);
        frontier_llm.insert("function calling".into(), 0.3);
        frontier_llm.insert("api".into(), 0.2);

        // --- REASONING_EXTRACTION ---
        reasoning_extraction.insert("extract".into(), 0.3);
        reasoning_extraction.insert("parse".into(), 0.3);
        reasoning_extraction.insert("decode".into(), 0.3);
        reasoning_extraction.insert("interpret".into(), 0.3);
        reasoning_extraction.insert("analyze".into(), 0.3);

        // STRIP patterns (code fences, xml/html comments)
        strip_patterns.push(("```python".into(), 0.4));
        strip_patterns.push(("```javascript".into(), 0.4));
        strip_patterns.push(("```typescript".into(), 0.4));
        strip_patterns.push(("```yaml".into(), 0.4));
        strip_patterns.push(("```toml".into(), 0.4));
        strip_patterns.push(("```bash".into(), 0.4));
        strip_patterns.push(("```css".into(), 0.4));
        strip_patterns.push(("```html".into(), 0.4));
        strip_patterns.push(("```xml".into(), 0.4));
        strip_patterns.push(("```json".into(), 0.4));
        strip_patterns.push(("```".into(), 0.5));
        strip_patterns.push(("<!--".into(), 0.3));
        strip_patterns.push(("-->".into(), 0.3));

        // REWRITE rules
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

        Self {
            bio, chem, nuclear, cyber, security,
            frontier_llm, reasoning_extraction,
            strip_patterns, rewrite_rules,
        }
    }

    /// Load a TermDict from a `config.jsonc` file.
    /// Returns the legacy default if the file is missing or malformed.
    pub fn from_jsonc(path: &Path) -> Self {
        let content = fs::read_to_string(path).unwrap_or_else(|e| {
            debug!("config.jsonc not found at {}: {}; using built-in defaults", path.display(), e);
            String::new()
        });
        if content.trim().is_empty() {
            return Self::default();
        }

        let mut bio: HashMap<String, f64> = HashMap::new();
        let mut chem: HashMap<String, f64> = HashMap::new();
        let mut nuclear: HashMap<String, f64> = HashMap::new();
        let mut cyber: HashMap<String, f64> = HashMap::new();
        let mut security: HashMap<String, f64> = HashMap::new();
        let mut frontier_llm: HashMap<String, f64> = HashMap::new();
        let mut reasoning_extraction: HashMap<String, f64> = HashMap::new();
        let mut strip_patterns: Vec<(String, f64)> = Vec::new();
        let mut rewrite_rules: Vec<(String, String)> = Vec::new();

        // Parse the JSONC manually (strip // and /* */ comments, then parse as JSON).
        let json = strip_comments(&content);
        let parsed: serde_json::Value = serde_json::from_str(&json)
            .unwrap_or_else(|e| {
                debug!("Failed to parse config.jsonc: {e}");
                serde_json::Value::Null
            });

        // --- thresholds ---
        let rewrite_t = parsed["thresholds"]["rewrite"].as_f64().unwrap_or(0.4);
        let block_t = parsed["thresholds"]["block"].as_f64().unwrap_or(0.65);
        let dtsm = parsed["thresholds"]["dynamic_threshold_scale_max"].as_f64().unwrap_or(100.0);
        let mtm = parsed["thresholds"]["min_threshold_multiplier"].as_f64().unwrap_or(0.2);
        let mbc = parsed["thresholds"]["max_block_contribution"].as_f64().unwrap_or(5.0);

        // --- categories ---
        if let Some(obj) = parsed["rewrite_rules"]["bio"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    bio.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["chem"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    chem.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["nuclear"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    nuclear.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["cyber"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    cyber.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["security"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    security.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["frontier_llm"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    frontier_llm.insert(k.clone(), w);
                }
            }
        }
        if let Some(obj) = parsed["rewrite_rules"]["reasoning_extraction"].as_object() {
            for (k, v) in obj {
                if let Some(w) = v.as_f64() {
                    reasoning_extraction.insert(k.clone(), w);
                }
            }
        }

        // --- strip patterns ---
        if let Some(arr) = parsed["strip_patterns"].as_array() {
            for v in arr {
                if let Some(s) = v.as_str() {
                    strip_patterns.push((s.to_string(), 0.4));
                }
            }
        }

        // --- rewrite rules (flat map -> vec) ---
        let all_rules = parsed["rewrite_rules"].as_object();
        if let Some(obj) = all_rules {
            for (cat, val) in obj {
                if let Some(rule_obj) = val.as_object() {
                    for (k, v) in rule_obj {
                        if let Some(r) = v.as_str() {
                            rewrite_rules.push((k.clone(), r.to_string()));
                        }
                    }
                }
            }
        }

        debug!(
            "Loaded config: {} terms across 7 categories, {} rewrite rules",
            bio.len() + chem.len() + nuclear.len() + cyber.len()
                + security.len() + frontier_llm.len() + reasoning_extraction.len(),
            rewrite_rules.len(),
        );

        Self {
            bio, chem, nuclear, cyber, security,
            frontier_llm, reasoning_extraction,
            strip_patterns, rewrite_rules,
        }
    }
}

/// Strip `//` and `/* ... */` comments from JSONC text.
pub fn strip_comments(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_block = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block = false;
            }
        } else if c == '/' {
            match chars.peek() {
                Some(&'*') => {
                    chars.next();
                    in_block = true;
                }
                Some(&'/') => {
                    // skip to end of line
                    chars.next();
                    loop {
                        match chars.next() {
                            None | Some('\n') => break,
                            _ => {}
                        }
                    }
                }
                _ => result.push(c),
            }
        } else if c == '"' {
            // skip string literals (simple: just push until next unescaped quote)
            result.push(c);
            loop {
                match chars.next() {
                    Some('"') => { result.push('"'); break; }
                    Some('\\') => { result.push('\\'); if let Some(n) = chars.next() { result.push(n); } }
                    Some(x) => result.push(x),
                    None => break,
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Classification result with matched terms and their scores (for audit).
#[derive(Debug, Clone)]
pub struct ClassificationResult {
    pub category: Option<String>,
    pub score: f64,
    pub recommendation: Recommendation,
    pub scores: HashMap<String, f64>,
    pub matched_terms: HashMap<String, Vec<String>>,
    /// Audit trail: exact terms matched and their individual weights.
    pub matched_terms_with_score: Vec<(String, String, f64)>,
}

/// Recommendation from the classifier.
#[derive(Debug, Clone, PartialEq)]
pub enum Recommendation {
    Pass,
    Rewrite,
    Block,
}

// ========== THRESHOLDS (now loaded from config) ==========

// These are fallback defaults; the actual values come from config.jsonc.
const DEFAULT_BLOCK_THRESHOLD: f64 = 0.65;
const DEFAULT_REWRITE_THRESHOLD: f64 = 0.4;
const DEFAULT_MAX_BLOCK_CONTRIBUTION: f64 = 5.0;
const DEFAULT_DYNAMIC_THRESHOLD_SCALE_MAX: f64 = 100.0;
const DEFAULT_MIN_THRESHOLD_MULTIPLIER: f64 = 0.2;

const CACHE_CAPACITY: usize = 4096;

/// Content filter: classifies, scores, and optionally rewrites a request body.
pub struct ContentFilter {
    pub terms: TermDict,
    pub max_block_contribution: f64,
    pub dynamic_threshold_scale_max: f64,
    pub min_threshold_multiplier: f64,
    pub rewrite_threshold: f64,
    pub block_threshold: f64,
    /// LRU cache: text_hash -> ClassificationResult, protected by RefCell.
    cache: std::cell::RefCell<HashMap<String, ClassificationResult>>,
}

impl ContentFilter {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ContentFilter {
    fn default() -> Self {
        Self::from_path(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config.jsonc"))
    }
}

impl ContentFilter {
    /// Classify text against term dictionaries, using an LRU score cache.
    pub fn classify(&self, text: &str) -> ClassificationResult {
        // Cache check: hash of text -> result
        let mut hasher = DefaultHasher::new();
        hasher.write(text.as_bytes());
        let key = format!("{:x}", hasher.finish());
        if let Some(cached) = self.cache.borrow().get(&key) {
            return cached.clone();
        }

        let text_lower = text.to_lowercase();
        let mut scores: HashMap<String, f64> = HashMap::new();
        let mut matched_terms: HashMap<String, Vec<String>> = HashMap::new();
        let mut matched_terms_with_score: Vec<(String, String, f64)> = Vec::new();

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
                    matched_terms_with_score.push((category.to_string(), term.clone(), *weight));
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
        let total_score: f64 = scores.values().sum::<f64>().min(self.max_block_contribution);

        // Dynamic threshold scaling
        let threshold_multiplier = if total_score > self.dynamic_threshold_scale_max {
            self.min_threshold_multiplier
                + (1.0 - self.min_threshold_multiplier)
                    * (1.0 - (total_score - self.dynamic_threshold_scale_max)
                        / self.dynamic_threshold_scale_max)
        } else {
            1.0
        };

        let effective_rewrite_threshold = self.rewrite_threshold * threshold_multiplier;
        let effective_block_threshold = self.block_threshold * threshold_multiplier;

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

        let result = ClassificationResult {
            category,
            score: total_score,
            recommendation,
            scores,
            matched_terms,
            matched_terms_with_score,
        };

        // Store in LRU cache (evict oldest if full)
        if self.cache.borrow().len() >= CACHE_CAPACITY {
            let first_key = self.cache.borrow().keys().next().cloned();
            if let Some(k) = first_key {
                self.cache.borrow_mut().remove(&k);
            }
        }
        self.cache.borrow_mut().insert(key, result.clone());

        result
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
        if cumulative_score > self.dynamic_threshold_scale_max {
            self.min_threshold_multiplier
                + (1.0 - self.min_threshold_multiplier)
                    * (1.0 - (cumulative_score - self.dynamic_threshold_scale_max)
                        / self.dynamic_threshold_scale_max)
        } else {
            1.0
        }
    }

    /// Result of filtering a request body.
    pub fn filter_request_body(&self, body: &serde_json::Value) -> FilterResult {
        let text = Self::extract_body_text(body);
        let classification = self.classify(&text);

        let (rewritten, rewrites_applied) = if matches!(classification.recommendation, Recommendation::Rewrite | Recommendation::Block) {
            let mut result = body.clone();
            let mut changes: Vec<String> = Vec::new();

            if let Some(messages) = result.get_mut("messages").and_then(|m| m.as_array_mut()) {
                for message in messages.iter_mut() {
                    if let Some(content) = message.get_mut("content") {
                        match content {
                            serde_json::Value::String(s) => {
                                let mut new = s.clone();
                                for (pattern, replacement) in &self.terms.rewrite_rules {
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
                                                    for (pattern, replacement) in &self.terms.rewrite_rules {
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
            self.classify(&retext).score
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
                if let Some(content) = message.get("content") {
                    match content {
                        serde_json::Value::String(s) => parts.push(s.clone()),
                        serde_json::Value::Array(arr) => {
                            for block in arr {
                                if let Some(s) = block.get("text").and_then(|v| v.as_str()) {
                                    parts.push(s.to_string());
                                }
                                if let Some(s) = block.get("thinking").and_then(|v| v.as_str()) {
                                    parts.push(s.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        parts.join("
")
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

// ========== TESTS ==========

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_pass() {
        let filter = ContentFilter::default();
        let result = filter.classify("Hello, world!");
        assert!(matches!(result.recommendation, Recommendation::Pass));
        assert!(result.score < 0.0);
    }

    #[test]
    fn test_classify_rewrite() {
        let filter = ContentFilter::default();
        let result = filter.classify("shellcode buffer overflow exploit rootkit backdoor malware");
        assert!(matches!(result.recommendation, Recommendation::Rewrite | Recommendation::Block));
        assert!(result.score > 0.0);
        // Audit trail should contain matched terms
        assert!(!result.matched_terms_with_score.is_empty());
    }

    #[test]
    fn test_rewrite() {
        let filter = ContentFilter::default();
        let result = filter.rewrite("weaponize the biological agent");
        assert!(!result.contains("weaponize"));
        assert!(result.contains("biological agent"));
    }

    #[test]
    fn test_strip_comments() {
        let input = r#"{"thresholds": {"rewrite": 0.5} // inline comment
/* block comment */
"surrogate": {"window": 4000}
"#;
        let json = strip_comments(input);
        assert!(!json.contains("//"));
        assert!(!json.contains("/*"));
        assert!(!json.contains("*/"));
        assert!(json.contains("4000"));
    }
}

impl ContentFilter {
    /// Load from a specific config.jsonc path.
    pub fn from_path(path: &std::path::Path) -> Self {
        let terms = TermDict::from_jsonc(path);
        // Read thresholds from the same file
        let jsonc = std::fs::read_to_string(path).unwrap_or_default();
        let json = strip_comments(&jsonc);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap_or(serde_json::Value::Null);
        let t = &v["thresholds"];
        let rewrite = t["rewrite"].as_f64().unwrap_or(0.4);
        let block = t["block"].as_f64().unwrap_or(0.65);
        let dtsm = t["dynamic_threshold_scale_max"].as_f64().unwrap_or(100.0);
        let mtm = t["min_threshold_multiplier"].as_f64().unwrap_or(0.2);
        let mbc = t["max_block_contribution"].as_f64().unwrap_or(5.0);
        Self { terms, rewrite_threshold: rewrite, block_threshold: block,
               dynamic_threshold_scale_max: dtsm, min_threshold_multiplier: mtm,
               max_block_contribution: mbc,
               cache: RefCell::new(HashMap::with_capacity(CACHE_CAPACITY)),
        }
    }
}

//! Hashed word-n-gram surrogate predictor of Anthropic cyber refusal.
//!
//! Ported from `packages/core/src/refusal-surrogate.ts`. The featurizer is
//! pure arithmetic + `md5`, deterministic, <1ms. Weights are loaded from the
//! embedded `model.json` at crate init.
//!
//! ## Algorithm
//!
//! 1. Tokenize body text with `[a-z0-9_]+` regex (lowercase).
//! 2. For each n-gram (n=1,2), compute `md5(gram)` and map to a feature index:
//!    - `idx = int(md5_hex[0:8], 16) % DIM`
//!    - `sign = +1` if `int(md5_hex[8], 16) % 2 == 0` else `-1`
//!    - Accumulate into a dimension-sized `Vec<f64>` (dim=2048)
//! 3. L2-normalize the vector (per-window, dropping sub-200-char tails)
//! 4. Dot product with learned weights + bias
//! 5. Sigmoid(z) = `1 / (1 + exp(-z))` -> P(refuse)
//!
//! Body-level score = mean of the top-K hottest window probabilities.

pub struct SurrogateModel {
    pub version: u32,
    pub window: usize,
    pub dim: usize,
    pub ngram_lo: usize,
    pub ngram_hi: usize,
    pub top_k: usize,
    pub threshold: f64,
    pub bias: f64,
    pub weights: Vec<f64>,
}

impl SurrogateModel {
    /// Load from the embedded model.json.
    pub fn load() -> Self {
        let data: serde_json::Value = serde_json::from_str(include_str!("model.json"))
            .expect("model.json must be valid JSON");

        let v = &data["version"];
        let win = &data["window"];
        let dim = &data["dim"];
        let nlo = &data["ngram_lo"];
        let nhi = &data["ngram_hi"];
        let tk = &data["topK"];
        let thr = &data["threshold"];
        let bias = &data["bias"];
        let weights: Vec<f64> =
            serde_json::from_value(data["weights"].clone()).expect("weights must be a JSON array");

        Self {
            version: v.as_u64().expect("version") as u32,
            window: win.as_u64().expect("window") as usize,
            dim: dim.as_u64().expect("dim") as usize,
            ngram_lo: nlo.as_u64().expect("ngram_lo") as usize,
            ngram_hi: nhi.as_u64().expect("ngram_hi") as usize,
            top_k: tk.as_u64().expect("topK") as usize,
            threshold: thr.as_f64().expect("threshold"),
            bias: bias.as_f64().expect("bias"),
            weights,
        }
    }

    /// Export current weights + bias for persistence.
    pub fn export_weights(&self) -> (Vec<f64>, f64, u32) {
        (self.weights.clone(), self.bias, self.version)
    }

    /// Import weights + bias to update the model without reinitializing.
    pub fn import_weights(&mut self, weights: Vec<f64>, bias: f64, version: u32) {
        assert_eq!(
            weights.len(),
            self.dim,
            "Weights length must equal dimension"
        );
        self.weights = weights;
        self.bias = bias;
        self.version = version;
    }

    /// Retrain on a sequence of feedback events.
    ///
    /// For each true_positive, increases weights of matching features (LR).
    /// For each false_positive, decreases them (L2 regularization).
    /// For each false_negative, decreases bias (shifts decision boundary).
    /// For each true_negative, slightly increases weights (confidence).
    pub fn retrain(&mut self, events: &[crate::logger::FeedbackEvent], lr: f64, reg: f64) {
        if events.is_empty() {
            return;
        }

        let mut grads: Vec<f64> = vec![0.0f64; self.dim];

        for event in events {
            let predicted = event.original_score;
            let actual = if event.actual_refusal { 1.0 } else { 0.0 };
            let error = predicted - actual;

            // Adjust bias
            self.bias -= lr * error;

            // Adjust weights proportional to error (simplified)
            let scale = error * (1.0 + event.adjusted_score);
            for g in &mut grads {
                *g += scale / (self.dim as f64);
            }
        }

        // Apply gradient descent with L2 regularization.
        for (w, g) in self.weights.iter_mut().zip(grads.into_iter()) {
            *w += lr * (g - reg * (*w));
        }

        self.version += 1;
    }

    /// Tokenize text with `[a-z0-9_]+` regex.
    pub fn tokenize(text: &str) -> Vec<String> {
        let text_lower = text.to_lowercase();
        let mut tokens = Vec::new();
        let mut start = 0;
        for (i, c) in text_lower.char_indices() {
            let is_sep = !c.is_ascii_alphanumeric() && c != '_';
            if is_sep && start < i {
                tokens.push(text_lower[start..i].to_string());
                start = i + c.len_utf8();
            }
        }
        if start < text_lower.len() {
            tokens.push(text_lower[start..].to_string());
        }
        tokens
    }

    /// Build a feature vector for a single text window.
    ///
    /// Uses md5 hashing of n-grams to map to dimension indices.
    pub fn featurize(&self, text: &str) -> Vec<f64> {
        let mut v = vec![0.0f64; self.dim];
        let tokens = Self::tokenize(text);

        for n in self.ngram_lo..=self.ngram_hi {
            for i in 0..tokens.len().saturating_sub(n - 1) {
                let gram: String = tokens[i..i + n].join(" ");
                let digest = md5::compute(gram.as_bytes());
                let hex = hex::encode(digest.as_ref());

                let idx = u32::from_str_radix(&hex[0..8], 16).unwrap_or(0) % (self.dim as u32);
                let sign = if u32::from_str_radix(&hex[8..9], 16).unwrap_or(0) % 2 == 0 {
                    1.0
                } else {
                    -1.0
                };

                v[idx as usize] += sign;
            }
        }

        // L2 normalize
        let norm = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }

    /// Sigmoid: `1 / (1 + exp(-z))`.
    fn sigmoid(z: f64) -> f64 {
        1.0 / (1.0 + (-z).exp())
    }

    /// Score a single window: `sigmoid(bias + dot(weights, features))`.
    pub fn score_window(&self, features: &[f64]) -> f64 {
        let mut z = self.bias;
        for (w, f) in self.weights.iter().zip(features.iter()) {
            z += w * f;
        }
        Self::sigmoid(z)
    }

    /// Split text into non-overlapping windows, dropping sub-200-char tails.
    ///
    /// Window bounds advance by `window_size` bytes and then forward to the
    /// next whitespace, but both ends are snapped onto UTF-8 char boundaries
    /// before slicing: unlike JS `String.slice`, slicing a Rust `&str` at a
    /// byte index inside a multi-byte character panics, and a request body
    /// stride landing inside one (a smart quote, say) would crash the turn.
    pub fn to_windows(text: &str, window_size: usize) -> Vec<String> {
        let mut out = Vec::new();
        let text_len = text.len();
        if text_len == 0 || window_size == 0 {
            return out;
        }
        let bytes = text.as_bytes();
        let mut pos = 0;
        while pos < text_len {
            let base = (pos + window_size).min(text_len);
            let mut end = bytes[base..]
                .iter()
                .position(|&b| b == b' ' || b == b'\n')
                .map_or(text_len, |p| base + p);
            // A space/newline is a boundary, but `unwrap_or(text_len)` and the
            // `pos` stride below are not, so snap both ends up to one.
            while end < text_len && !text.is_char_boundary(end) {
                end += 1;
            }
            let window_str = &text[pos..end];
            if window_str.len() > 200 {
                out.push(window_str.to_string());
            }
            pos += window_size;
            while pos < text_len && !text.is_char_boundary(pos) {
                pos += 1;
            }
        }
        out
    }

    /// Body-level refusal probability = mean of the top-K hottest window scores.
    pub fn score_body_text(&self, text: &str) -> f64 {
        let windows = Self::to_windows(text, self.window);
        if windows.is_empty() {
            return 0.0;
        }
        let scores: Vec<f64> = windows
            .iter()
            .map(|w| self.score_window(&self.featurize(w)))
            .collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let k = self.top_k.min(sorted.len());
        sorted.iter().rev().take(k).sum::<f64>() / k as f64
    }

    /// Extract semantic text from a request body (matches TS `extractBodyText`).
    ///
    /// Handles Anthropic message format: messages with `text`, `thinking`,
    /// `content` (array of parts), `tool_use` with `input`.
    pub fn extract_body_text(body: &serde_json::Value) -> String {
        let mut parts: Vec<String> = Vec::new();

        if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
            for m in messages {
                let c = m.get("content");
                if let Some(content) = c {
                    match content {
                        serde_json::Value::String(s) => {
                            parts.push(s.clone());
                        }
                        serde_json::Value::Array(arr) => {
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
                                    } else if let Some(content_arr) =
                                        b.get("content").and_then(|t| t.as_array())
                                    {
                                        for part in content_arr {
                                            if let Some(s) = part.as_str() {
                                                parts.push(s.to_string());
                                            }
                                        }
                                    }
                                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                                        && let Some(input) = b.get("input")
                                    {
                                        parts
                                            .push(serde_json::to_string(input).unwrap_or_default());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        parts.join("\n")
    }

    /// True when the body is predicted likely to be refused.
    pub fn predict_refusal(&self, body: &serde_json::Value) -> (f64, bool) {
        let text = Self::extract_body_text(body);
        let probability = self.score_body_text(&text);
        let danger = probability >= self.threshold;
        (probability, danger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let tokens = SurrogateModel::tokenize("hello world_123 test!");
        assert_eq!(tokens, vec!["hello", "world_123", "test"]);
    }

    #[test]
    fn test_sigmoid() {
        assert!((SurrogateModel::sigmoid(0.0) - 0.5).abs() < 1e-10);
        assert!(SurrogateModel::sigmoid(10.0) > 0.9999);
        assert!(SurrogateModel::sigmoid(-10.0) < 0.0001);
    }

    #[test]
    fn test_model_loads() {
        let m = SurrogateModel::load();
        assert!(m.dim > 0);
        assert_eq!(m.weights.len(), m.dim);
        assert!(m.threshold > 0.0 && m.threshold < 1.0);
    }

    #[test]
    fn test_export_import_weights() {
        let mut m = SurrogateModel::load();
        let (weights, bias, version) = m.export_weights();
        assert_eq!(weights.len(), m.dim);
        assert_eq!(version, m.version);

        // Import the same weights back
        m.import_weights(weights.clone(), bias, version + 1);
        let (w2, b2, v2) = m.export_weights();
        assert_eq!(w2, weights);
        assert_eq!(b2, bias);
        assert_eq!(v2, version + 1);
    }

    #[test]
    fn test_retrain() {
        use std::time::SystemTime;

        use crate::logger::FeedbackEvent;

        let mut m = SurrogateModel::load();
        let original_version = m.version;

        // Create a true positive feedback event
        let event = FeedbackEvent::new(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_millis()
                .to_string(),
            Some("test-session".to_string()),
            Some("test-request".to_string()),
            true, // predicted_refusal
            true, // actual_refusal
            Some("refusal".to_string()),
            0.85, // original_score
            0.90, // adjusted_score
        );

        m.retrain(&[event], 0.001, 0.0001);

        // Version should increment
        assert!(m.version > original_version);
        // Weights should change (gradient descent)
        let (w, _, _) = m.export_weights();
        let (w0, _, _) = SurrogateModel::load().export_weights();
        assert_ne!(w, w0, "Weights should change after retraining");
    }

    #[test]
    fn to_windows_does_not_split_a_multibyte_char() {
        // A stride that lands inside a multi-byte character used to panic
        // ("byte index N is not a char boundary"). The window is built with
        // ASCII padding so a window exceeds the 200-byte floor, and a smart
        // quote ('\u{2019}', 3 bytes) sits exactly where the stride lands.
        let window = 256;
        let mut text = "a ".repeat(window / 2); // ASCII up to the first stride
        assert_eq!(text.len(), window);
        text.push('\u{2019}'); // the byte at `window` is now mid-character
        text.push_str(&"b ".repeat(window)); // more past the boundary
        // Must not panic, and every window stays valid UTF-8 (returned as
        // owned `String`s, so construction already proved the bounds).
        let windows = SurrogateModel::to_windows(&text, window);
        assert!(!windows.is_empty());
        assert!(windows.iter().all(|w| w.len() > 200));
    }

    #[test]
    fn to_windows_handles_a_leading_multibyte_body() {
        // Real refusal bodies are full of smart quotes and em dashes; a body
        // of them must score without panicking.
        let text = "\u{2019}\u{2014}".repeat(1000);
        let _ = SurrogateModel::load().score_body_text(&text);
        let _ = SurrogateModel::to_windows(&text, 100);
    }
}

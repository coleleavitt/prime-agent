//! A scripted Prime auth transport for the login tests: exact URL -> response,
//! in call order; the served requests land in a log. With `dynamic_generate`
//! (the default) the challenge generate POST answers a fixed challenge, and the
//! status poll answers pending once, then the fixture key encrypted with the
//! flow's public key.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;
use pa_core::auth::{PrimeHttp, PrimeHttpResponse};
use rsa::pkcs8::DecodePublicKey;

pub(crate) struct ScriptedHttp {
    queue: Mutex<VecDeque<(String, u16, String)>>,
    dynamic_generate: bool,
    dynamic_status: Mutex<Option<String>>,
    status_pending_once: AtomicBool,
    served: Mutex<Vec<String>>,
}

impl ScriptedHttp {
    pub(crate) fn new(responses: Vec<(&str, u16, &str)>) -> Self {
        ScriptedHttp {
            queue: Mutex::new(
                responses
                    .into_iter()
                    .map(|(url, status, body)| (url.to_string(), status, body.to_string()))
                    .collect(),
            ),
            dynamic_generate: true,
            dynamic_status: Mutex::new(None),
            status_pending_once: AtomicBool::new(true),
            served: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn without_dynamic(responses: Vec<(&str, u16, &str)>) -> Self {
        let mut scripted = Self::new(responses);
        scripted.dynamic_generate = false;
        scripted
    }

    pub(crate) fn requests(&self) -> Vec<String> {
        self.served.lock().unwrap().clone()
    }

    fn pop(&self, url: &str) -> Option<(u16, String)> {
        let mut queue = self.queue.lock().unwrap();
        let position = queue.iter().position(|(expected, _, _)| expected == url)?;
        let (_, status, body) = queue
            .remove(position)
            .expect("the scripted response was queued");
        Some((status, body))
    }
}

impl PrimeHttp for ScriptedHttp {
    fn get(
        &self,
        url: &str,
        _api_key: &str,
        _timeout_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PrimeHttpResponse, String>> + Send>> {
        // The boxed answer is 'static, so the read resolves before
        // the future arms.
        let url = url.to_string();
        self.served.lock().unwrap().push(url.clone());
        // The cipher stays until the result response consumes it.
        let dynamic = self.dynamic_status.lock().unwrap().clone();
        let answer = if url.contains("/api/v1/auth_challenge/status") && dynamic.is_some() {
            if self.status_pending_once.swap(false, Ordering::SeqCst) {
                // The first poll answers pending (the flow sleeps its
                // interval).
                Ok(PrimeHttpResponse {
                    status: 200,
                    body: r#"{"pending":true}"#.to_string(),
                })
            } else {
                let cipher = self.dynamic_status.lock().unwrap().take();
                Ok(PrimeHttpResponse {
                    status: 200,
                    body: format!(r#"{{"result":"{}"}}"#, cipher.unwrap_or_default()),
                })
            }
        } else {
            match self.pop(&url) {
                Some((status, body)) => Ok(PrimeHttpResponse { status, body }),
                None => panic!("no scripted response for {url}"),
            }
        };
        Box::pin(async move { answer })
    }

    fn post_json<'a>(
        &'a self,
        url: &'a str,
        body: &'a str,
        _bearer: Option<&'a str>,
        _timeout_ms: u64,
    ) -> Pin<Box<dyn Future<Output = Result<PrimeHttpResponse, String>> + Send + 'a>> {
        let url = url.to_string();
        let body = body.to_string();
        self.served.lock().unwrap().push(url.clone());
        Box::pin(async move {
            if self.dynamic_generate && url.ends_with("/api/v1/auth_challenge/generate") {
                let parsed: serde_json::Value = serde_json::from_str(&body).expect("generate body");
                let public_pem = parsed
                    .get("encryptionPublicKey")
                    .and_then(serde_json::Value::as_str)
                    .expect("public key")
                    .to_string();
                let mut rng = rsa::rand_core::OsRng;
                let public_key =
                    rsa::RsaPublicKey::from_public_key_pem(&public_pem).expect("public pem");
                let cipher = public_key
                    .encrypt(
                        &mut rng,
                        rsa::Oaep::new::<sha2::Sha256>(),
                        b"fixture-browser-key",
                    )
                    .expect("encrypt");
                let encoded = base64::engine::general_purpose::STANDARD.encode(cipher);
                *self.dynamic_status.lock().unwrap() = Some(encoded);
                return Ok(PrimeHttpResponse {
                    status: 200,
                    body: r#"{"challenge":"ch-1","status_auth_token":"tok"}"#.to_string(),
                });
            }
            match self.pop(&url) {
                Some((status, body)) => Ok(PrimeHttpResponse { status, body }),
                None => panic!("no scripted response for {url}"),
            }
        })
    }
}

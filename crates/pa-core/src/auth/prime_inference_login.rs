//! Prime Inference login orchestration: the production prime-cli credential reuse, the browser
//! challenge over `prime_traces.rs`'s `run_prime_browser_login`, and the inference access check.
//! The interactive surface is the composition root's.

use std::path::Path;

use super::prime_inference::{
    DEFAULT_REQUEST_TIMEOUT_MS,
    PrimeAccessError,
    PrimeHttp,
    PrimeInferenceAuthConfig,
    check_prime_inference_access,
    read_prime_cli_config,
};
use super::prime_traces::{DEFAULT_POLL_INTERVAL_MS, PrimeAuthInfo, run_prime_browser_login};
use super::types::PrimeTeamAssignment;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimeInferenceLoginSource {
    PrimeCli,
    Browser,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrimeInferenceLoginResult {
    pub api_key: String,
    pub source: PrimeInferenceLoginSource,
    /// The TS `primeTeam` tri-state: the cli candidate's team, `null`
    /// for its personal account, absent for the browser arm.
    pub prime_team: PrimeTeamAssignment,
}

pub struct PrimeInferenceLoginCallbacks<'a> {
    pub on_auth: &'a (dyn Fn(&PrimeAuthInfo) + Send + Sync),
    pub on_progress: Option<&'a (dyn Fn(&str) + Send + Sync)>,
}

impl PrimeInferenceLoginCallbacks<'_> {
    fn progress(&self, message: &str) {
        if let Some(on_progress) = self.on_progress {
            on_progress(message);
        }
    }
}

pub struct PrimeInferenceLoginOptions<'a> {
    pub prime_cli_config_path: Option<&'a Path>,
    pub use_prime_cli_config: bool,
    pub poll_interval_ms: Option<u64>,
    pub request_timeout_ms: Option<u64>,
}

impl<'a> PrimeInferenceLoginOptions<'a> {
    #[must_use]
    pub fn new(prime_cli_config_path: Option<&'a Path>) -> Self {
        PrimeInferenceLoginOptions {
            prime_cli_config_path,
            use_prime_cli_config: prime_cli_config_path.is_some(),
            poll_interval_ms: None,
            request_timeout_ms: None,
        }
    }
}

/// The whole login: the production prime-cli credential reuse first, then the
/// browser challenge (its URL carries only the code), then the inference access
/// check. TS's `signal` cancellation is the caller dropping this future.
///
/// # Errors
///
/// Returns a human-readable error string when access check fails to run or the resulting key lacks
/// Prime Inference access.
pub async fn login_prime_inference(
    http: &dyn PrimeHttp,
    config: &PrimeInferenceAuthConfig,
    options: &PrimeInferenceLoginOptions<'_>,
    callbacks: &PrimeInferenceLoginCallbacks<'_>,
) -> Result<PrimeInferenceLoginResult, String> {
    let request_timeout_ms = options
        .request_timeout_ms
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS);
    let poll_interval_ms = options.poll_interval_ms.unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    let candidate = if options.use_prime_cli_config && config.is_production() {
        options
            .prime_cli_config_path
            .and_then(read_prime_cli_config)
    } else {
        None
    };
    if let Some(api_key) = candidate
        .as_ref()
        .and_then(|candidate| candidate.api_key.as_deref())
    {
        callbacks.progress("Checking existing Prime CLI credentials...");
        match check_prime_inference_access(http, &config.base_url, api_key, request_timeout_ms)
            .await
        {
            Ok(()) => {
                let api_key = api_key.to_string();
                let prime_team = match candidate.and_then(|candidate| candidate.team) {
                    Some(team) => PrimeTeamAssignment::Team(team),
                    None => PrimeTeamAssignment::PersonalAccount,
                };
                return Ok(PrimeInferenceLoginResult {
                    api_key,
                    source: PrimeInferenceLoginSource::PrimeCli,
                    prime_team,
                });
            }
            // TS continues to the browser login with this line.
            Err(PrimeAccessError::Denied(failure)) => callbacks.progress(&format!(
                "Existing Prime CLI key cannot access Prime Inference ({}). Starting browser login...",
                failure.format()
            )),
            Err(PrimeAccessError::Failed(message)) => return Err(message),
        }
    } else {
        callbacks
            .progress("No eligible production Prime CLI API key found. Starting browser login...");
    }
    // No scope on the URL: the inference challenge carries only the code.
    let api_key = run_prime_browser_login(
        http,
        &config.base_url,
        &config.frontend_url,
        None,
        callbacks.on_auth,
        request_timeout_ms,
        poll_interval_ms,
    )
    .await?;
    callbacks.progress("Checking Prime Inference access...");
    match check_prime_inference_access(http, &config.base_url, &api_key, request_timeout_ms).await {
        Ok(()) => Ok(PrimeInferenceLoginResult {
            api_key,
            source: PrimeInferenceLoginSource::Browser,
            // The browser result carries no `primeTeam`: the stored
            // selection survives the write.
            prime_team: PrimeTeamAssignment::PreserveWhenKeyMatches,
        }),
        Err(PrimeAccessError::Denied(failure)) => Err(format!(
            "Prime API key does not have Prime Inference access ({})",
            failure.format()
        )),
        Err(PrimeAccessError::Failed(message)) => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::prime_inference::{DEFAULT_PRIME_API_BASE_URL, DEFAULT_PRIME_FRONTEND_URL};
    use super::super::scripted_http::ScriptedHttp;
    use super::super::types::PrimeTeamCredential;
    use super::*;

    /// The production challenge config.
    fn production_config() -> PrimeInferenceAuthConfig {
        PrimeInferenceAuthConfig {
            base_url: DEFAULT_PRIME_API_BASE_URL.to_string(),
            frontend_url: DEFAULT_PRIME_FRONTEND_URL.to_string(),
        }
    }

    fn whoami_ok(scope_write: bool) -> (&'static str, u16, &'static str) {
        let body = if scope_write {
            r#"{"data":{"scope":{"inference":{"write":true}}}}"#
        } else {
            r#"{"data":{"scope":{"inference":{"write":false}}}}"#
        };
        (
            "https://api.primeintellect.ai/api/v1/user/whoami",
            200,
            body,
        )
    }

    /// Fast options: the poll interval stays real but short.
    fn fast_options(
        prime_cli_config_path: Option<&std::path::Path>,
    ) -> PrimeInferenceLoginOptions<'_> {
        let mut options = PrimeInferenceLoginOptions::new(prime_cli_config_path);
        options.poll_interval_ms = Some(10);
        options
    }

    #[tokio::test]
    async fn the_prime_cli_candidate_short_circuits_with_its_team() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{"api_key":"cli-key","team_id":"t-1","team_name":"One"}"#,
        )
        .expect("config");
        let http = ScriptedHttp::new(vec![whoami_ok(true)]);
        let progress = Mutex::new(Vec::<String>::new());
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|_| panic!("the cli path never shows a URL"),
            on_progress: Some(&|message: &str| {
                progress.lock().unwrap().push(message.to_string());
            }),
        };
        let result = login_prime_inference(
            &http,
            &production_config(),
            &fast_options(Some(&config_path)),
            &callbacks,
        )
        .await
        .expect("login");
        assert_eq!(
            result,
            PrimeInferenceLoginResult {
                api_key: "cli-key".to_string(),
                source: PrimeInferenceLoginSource::PrimeCli,
                prime_team: PrimeTeamAssignment::Team(PrimeTeamCredential {
                    team_id: "t-1".to_string(),
                    name: "One".to_string(),
                    slug: None,
                    role: None,
                    created_at: None,
                }),
            }
        );
        assert_eq!(
            progress.lock().unwrap()[..],
            ["Checking existing Prime CLI credentials...".to_string()]
        );
        assert_eq!(
            http.requests(),
            vec!["https://api.primeintellect.ai/api/v1/user/whoami".to_string()]
        );
    }

    #[tokio::test]
    async fn the_prime_cli_candidate_without_a_team_is_the_personal_account() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, r#"{"api_key":"cli-key"}"#).expect("config");
        let http = ScriptedHttp::new(vec![whoami_ok(true)]);
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|_| panic!("the cli path never shows a URL"),
            on_progress: None,
        };
        let result = login_prime_inference(
            &http,
            &production_config(),
            &fast_options(Some(&config_path)),
            &callbacks,
        )
        .await
        .expect("login");
        assert_eq!(result.prime_team, PrimeTeamAssignment::PersonalAccount);
    }

    #[tokio::test]
    async fn a_denied_cli_key_runs_the_browser_challenge() {
        let dir = tempfile::tempdir().expect("temp dir");
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, r#"{"api_key":"cli-key"}"#).expect("config");
        // The cli key is denied; the browser key passes.
        let http = ScriptedHttp::new(vec![
            (
                "https://api.primeintellect.ai/api/v1/user/whoami",
                403,
                r#"{"error":{"message":"denied"}}"#,
            ),
            whoami_ok(true),
        ]);
        let progress = Mutex::new(Vec::<String>::new());
        let auth = Mutex::new(Vec::<String>::new());
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|info: &PrimeAuthInfo| {
                auth.lock()
                    .unwrap()
                    .push(format!("{}\n{}", info.url, info.instructions));
            },
            on_progress: Some(&|message: &str| {
                progress.lock().unwrap().push(message.to_string());
            }),
        };
        let result = login_prime_inference(
            &http,
            &production_config(),
            &fast_options(Some(&config_path)),
            &callbacks,
        )
        .await
        .expect("login");
        assert_eq!(
            result,
            PrimeInferenceLoginResult {
                api_key: "fixture-browser-key".to_string(),
                source: PrimeInferenceLoginSource::Browser,
                prime_team: PrimeTeamAssignment::PreserveWhenKeyMatches,
            }
        );
        assert_eq!(
            progress.lock().unwrap()[..],
            [
                "Checking existing Prime CLI credentials...".to_string(),
                "Existing Prime CLI key cannot access Prime Inference (HTTP 403: denied). Starting browser login..."
                    .to_string(),
                "Checking Prime Inference access...".to_string(),
            ]
        );
        assert_eq!(
            http.requests(),
            vec![
                "https://api.primeintellect.ai/api/v1/user/whoami".to_string(),
                "https://api.primeintellect.ai/api/v1/auth_challenge/generate".to_string(),
                "https://api.primeintellect.ai/api/v1/auth_challenge/status?challenge=ch-1"
                    .to_string(),
                "https://api.primeintellect.ai/api/v1/auth_challenge/status?challenge=ch-1"
                    .to_string(),
                "https://api.primeintellect.ai/api/v1/user/whoami".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_browser_challenge_url_carries_only_the_code() {
        let http = ScriptedHttp::new(vec![whoami_ok(true)]);
        let progress = Mutex::new(Vec::<String>::new());
        let auth = Mutex::new(Vec::<String>::new());
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|info: &PrimeAuthInfo| {
                auth.lock()
                    .unwrap()
                    .push(format!("{}\n{}", info.url, info.instructions));
            },
            on_progress: Some(&|message: &str| {
                progress.lock().unwrap().push(message.to_string());
            }),
        };
        let result =
            login_prime_inference(&http, &production_config(), &fast_options(None), &callbacks)
                .await
                .expect("login");
        assert_eq!(result.source, PrimeInferenceLoginSource::Browser);
        // TS sends no scope for the inference arm: the URL keeps only
        // the code.
        assert_eq!(
            auth.lock().unwrap().join("\n"),
            "https://app.primeintellect.ai/dashboard/tokens/challenge?code=ch-1\nCode: ch-1"
        );
        assert_eq!(
            progress.lock().unwrap()[..],
            [
                "No eligible production Prime CLI API key found. Starting browser login..."
                    .to_string(),
                "Checking Prime Inference access...".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_non_production_config_never_reads_the_prime_cli() {
        let dir = tempfile::tempdir().expect("temp dir");
        // The cli config carries a working key, but the base URL is
        // overridden: the reuse is production-only.
        let config_path = dir.path().join("config.json");
        std::fs::write(&config_path, r#"{"api_key":"cli-key"}"#).expect("config");
        let config = PrimeInferenceAuthConfig {
            base_url: "https://api.example".to_string(),
            frontend_url: DEFAULT_PRIME_FRONTEND_URL.to_string(),
        };
        let http = ScriptedHttp::new(vec![(
            "https://api.example/api/v1/user/whoami",
            200,
            r#"{"data":{"scope":{"inference":{"write":true}}}}"#,
        )]);
        let progress = Mutex::new(Vec::<String>::new());
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|_| {},
            on_progress: Some(&|message: &str| {
                progress.lock().unwrap().push(message.to_string());
            }),
        };
        let result = login_prime_inference(
            &http,
            &config,
            &fast_options(Some(&config_path)),
            &callbacks,
        )
        .await
        .expect("login");
        assert_eq!(result.source, PrimeInferenceLoginSource::Browser);
        assert_eq!(
            progress.lock().unwrap()[..],
            [
                "No eligible production Prime CLI API key found. Starting browser login..."
                    .to_string(),
                "Checking Prime Inference access...".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn the_browser_key_without_inference_access_reports_the_ts_error() {
        let http = ScriptedHttp::new(vec![whoami_ok(false)]);
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|_| {},
            on_progress: None,
        };
        match login_prime_inference(&http, &production_config(), &fast_options(None), &callbacks)
            .await
        {
            Err(error) => assert_eq!(
                error,
                "Prime API key does not have Prime Inference access (Prime token does not have inference write permission)"
            ),
            Ok(result) => panic!("expected a denial, got {result:?}"),
        }
    }

    #[tokio::test]
    async fn an_expired_challenge_reports_the_ts_error() {
        // The dynamic generate stays off so the queue owns every answer.
        let http = ScriptedHttp::without_dynamic(vec![
            (
                "https://api.primeintellect.ai/api/v1/auth_challenge/generate",
                200,
                r#"{"challenge":"ch-1","status_auth_token":"tok"}"#,
            ),
            (
                "https://api.primeintellect.ai/api/v1/auth_challenge/status?challenge=ch-1",
                404,
                "",
            ),
        ]);
        let callbacks = PrimeInferenceLoginCallbacks {
            on_auth: &|_| {},
            on_progress: None,
        };
        let options = PrimeInferenceLoginOptions {
            prime_cli_config_path: None,
            use_prime_cli_config: false,
            poll_interval_ms: Some(0),
            request_timeout_ms: Some(DEFAULT_REQUEST_TIMEOUT_MS),
        };
        match login_prime_inference(&http, &production_config(), &options, &callbacks).await {
            Err(error) => assert_eq!(error, "Prime login challenge expired"),
            Ok(result) => panic!("expected an expiry, got {result:?}"),
        }
    }
}

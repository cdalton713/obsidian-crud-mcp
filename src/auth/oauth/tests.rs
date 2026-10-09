use std::path::Path;

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;

const PASSWORD: &str = "test-password";
const BASE_URL: &str = "https://example.com";
const REDIRECT_URI: &str = "https://app.example.com/callback";
const HOUR_MS: i64 = 3600 * 1000;

static HIDDEN_FIELD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"name="(\w+)"\s+value="([^"]*)""#).unwrap());

/// One `name=value` pair of a form body.
type Field<'a> = (&'a str, &'a str);

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("expected JSON, got {:?}: {e}", self.body))
    }

    fn error(&self) -> String {
        self.json()["error"].as_str().unwrap_or_default().to_owned()
    }

    fn location(&self) -> url::Url {
        let location = self.headers.get(header::LOCATION).expect("redirect has a Location header");
        url::Url::parse(location.to_str().unwrap()).unwrap()
    }

    /// Hidden `name="..." value="..."` inputs of the password page.
    fn hidden_fields(&self) -> HashMap<String, String> {
        HIDDEN_FIELD.captures_iter(&self.body).map(|c| (c[1].to_owned(), c[2].to_owned())).collect()
    }
}

fn query_param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

#[derive(Clone)]
struct Client {
    id: String,
    secret: Option<String>,
}

struct Pkce {
    verifier: String,
    challenge: String,
}

fn pkce() -> Pkce {
    let verifier = URL_SAFE_NO_PAD.encode(new_secret());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

/// An HTTP Basic header the way `client_secret_basic` clients build it.
fn basic(client_id: &str, secret: &str) -> String {
    let encode = crate::util::encode_uri_component;
    format!("Basic {}", STANDARD.encode(format!("{}:{}", encode(client_id), encode(secret))))
}

/// An approved authorization code, ready to exchange.
struct Approved {
    client: Client,
    pkce: Pkce,
    code: String,
}

struct Tokens {
    client: Client,
    access_token: String,
    refresh_token: String,
}

struct Harness {
    auth: Arc<OAuthProvider>,
    router: Router,
}

impl Harness {
    fn new() -> Self {
        Self::with(PASSWORD, None)
    }

    fn with(password: &str, persist_path: Option<PathBuf>) -> Self {
        let auth = OAuthProvider::new(BASE_URL, password, persist_path, 14);
        let router = auth.router();
        Self { auth, router }
    }

    fn persisted(path: &Path) -> Self {
        Self::with(PASSWORD, Some(path.to_owned()))
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply { status, headers, body: String::from_utf8_lossy(&bytes).into_owned() }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn post_json(&self, uri: &str, body: &str) -> Reply {
        let request =
            Request::post(uri).header(header::CONTENT_TYPE, "application/json").body(Body::from(body.to_owned()));
        self.send(request.unwrap()).await
    }

    async fn post_form(&self, uri: &str, fields: &[(&str, &str)], authorization: Option<&str>) -> Reply {
        let mut request = Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(authorization) = authorization {
            request = request.header(header::AUTHORIZATION, authorization);
        }
        let body = serde_urlencoded::to_string(fields).unwrap();
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    async fn register(&self, metadata: Value) -> Reply {
        self.post_json("/oauth/register", &metadata.to_string()).await
    }

    async fn register_client_for(&self, redirect_uri: &str) -> Client {
        let reply = self.register(json!({ "client_name": "test", "redirect_uris": [redirect_uri] })).await;
        assert_eq!(reply.status, StatusCode::CREATED, "registration failed: {}", reply.body);
        let body = reply.json();
        Client {
            id: body["client_id"].as_str().unwrap().to_owned(),
            secret: body["client_secret"].as_str().map(str::to_owned),
        }
    }

    async fn register_client(&self) -> Client {
        self.register_client_for(REDIRECT_URI).await
    }

    async fn register_public_client(&self) -> Client {
        let reply =
            self.register(json!({ "redirect_uris": [REDIRECT_URI], "token_endpoint_auth_method": "none" })).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        Client { id: reply.json()["client_id"].as_str().unwrap().to_owned(), secret: None }
    }

    async fn authorize_page_for(&self, client_id: &str, challenge: &str, redirect_uri: &str) -> Reply {
        let query = serde_urlencoded::to_string([
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", "test-state"),
            ("response_type", "code"),
        ])
        .unwrap();
        self.get(&format!("/oauth/authorize?{query}")).await
    }

    async fn authorize_page(&self, client_id: &str, challenge: &str) -> Reply {
        self.authorize_page_for(client_id, challenge, REDIRECT_URI).await
    }

    async fn submit_password(&self, code: &str, csrf: &str, password: &str) -> Reply {
        self.post_form("/oauth/approve", &[("code", code), ("csrf", csrf), ("password", password)], None).await
    }

    /// Open a fresh authorize page and submit `password` on it.
    async fn attempt_password(&self, client: &Client, password: &str) -> Reply {
        let fields = self.authorize_page(&client.id, &pkce().challenge).await.hidden_fields();
        self.submit_password(&fields["code"], &fields["csrf"], password).await
    }

    async fn approved_code_for(&self, client: Client) -> Approved {
        let pkce = pkce();
        let fields = self.authorize_page(&client.id, &pkce.challenge).await.hidden_fields();
        let reply = self.submit_password(&fields["code"], &fields["csrf"], PASSWORD).await;
        assert_eq!(reply.status, StatusCode::FOUND, "approve should redirect: {}", reply.body);
        let code = query_param(&reply.location(), "code").expect("redirect carries the code");
        Approved { client, pkce, code }
    }

    async fn approved_code(&self) -> Approved {
        let client = self.register_client().await;
        self.approved_code_for(client).await
    }

    async fn token(&self, fields: &[(&str, &str)], authorization: Option<&str>) -> Reply {
        self.post_form("/oauth/token", fields, authorization).await
    }

    /// Exchange an approved code with credentials in the body.
    async fn exchange(&self, approved: &Approved, verifier: Option<&str>) -> Reply {
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("code", approved.code.as_str()),
            ("client_id", approved.client.id.as_str()),
            ("redirect_uri", REDIRECT_URI),
        ];
        if let Some(secret) = &approved.client.secret {
            fields.push(("client_secret", secret));
        }
        if let Some(verifier) = verifier {
            fields.push(("code_verifier", verifier));
        }
        self.token(&fields, None).await
    }

    async fn refresh(&self, refresh_token: &str, client: &Client) -> Reply {
        let mut fields =
            vec![("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", &client.id)];
        if let Some(secret) = &client.secret {
            fields.push(("client_secret", secret));
        }
        self.token(&fields, None).await
    }

    async fn complete_flow(&self) -> Tokens {
        let approved = self.approved_code().await;
        let reply = self.exchange(&approved, Some(&approved.pkce.verifier)).await;
        assert_eq!(reply.status, StatusCode::OK, "token exchange failed: {}", reply.body);
        let body = reply.json();
        Tokens {
            client: approved.client,
            access_token: body["access_token"].as_str().unwrap().to_owned(),
            refresh_token: body["refresh_token"].as_str().unwrap().to_owned(),
        }
    }

    fn is_valid(&self, access_token: &str) -> bool {
        self.auth.validate_token(Some(&format!("Bearer {access_token}")))
    }
}

// --- Discovery ---

#[tokio::test]
async fn serves_protected_resource_metadata() {
    let body = Harness::new().get("/.well-known/oauth-protected-resource").await.json();
    assert_eq!(body["resource"], BASE_URL);
    assert_eq!(body["authorization_servers"], json!([BASE_URL]));
}

#[tokio::test]
async fn serves_authorization_server_metadata_with_s256() {
    let body = Harness::new().get("/.well-known/oauth-authorization-server").await.json();
    assert_eq!(body["code_challenge_methods_supported"], json!(["S256"]));
    assert_eq!(body["token_endpoint"], format!("{BASE_URL}/oauth/token"));
    assert_eq!(body["registration_endpoint"], format!("{BASE_URL}/oauth/register"));
    assert_eq!(body["authorization_endpoint"], format!("{BASE_URL}/oauth/authorize"));
}

// --- Dynamic client registration ---

#[tokio::test]
async fn registration_issues_a_secret_to_confidential_clients() {
    let reply = Harness::new().register(json!({ "client_name": "test", "redirect_uris": ["https://x.com/cb"] })).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let body = reply.json();
    assert!(body["client_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(body["client_secret"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(body["redirect_uris"], json!(["https://x.com/cb"]));
    assert_eq!(body["token_endpoint_auth_method"], "client_secret_post");
}

#[tokio::test]
async fn registration_with_auth_method_none_issues_no_secret() {
    let reply = Harness::new()
        .register(json!({
            "client_name": "test",
            "redirect_uris": ["https://x.com/cb"],
            "token_endpoint_auth_method": "none",
        }))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let body = reply.json();
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert!(body.get("client_secret").is_none(), "public client got a secret: {body}");
}

#[tokio::test]
async fn registration_falls_back_to_client_secret_post_for_unknown_methods() {
    let reply = Harness::new()
        .register(json!({
            "client_name": "test",
            "redirect_uris": ["https://x.com/cb"],
            "token_endpoint_auth_method": "client_secret_basic",
        }))
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let body = reply.json();
    assert_eq!(body["token_endpoint_auth_method"], "client_secret_post");
    assert!(body["client_secret"].is_string());
}

#[tokio::test]
async fn registration_keeps_at_most_five_redirect_uris_and_256_name_characters() {
    let uris: Vec<String> = (0..7).map(|i| format!("https://x.com/{i}")).collect();
    let reply = Harness::new().register(json!({ "client_name": "é".repeat(300), "redirect_uris": uris })).await;
    let body = reply.json();
    assert_eq!(body["redirect_uris"], json!(uris[..5]));
    assert_eq!(body["client_name"].as_str().unwrap().chars().count(), 256);
}

#[tokio::test]
async fn registration_rejects_malformed_json() {
    let reply = Harness::new().post_json("/oauth/register", "{not json").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "invalid_client_metadata");
}

#[tokio::test]
async fn registration_rejects_missing_or_unsafe_redirect_uris() {
    let harness = Harness::new();
    for metadata in [
        json!({ "client_name": "test" }),
        json!({ "redirect_uris": [] }),
        json!({ "redirect_uris": ["javascript:alert(1)"] }),
        json!({ "redirect_uris": ["https://ok.example/cb", "DATA:text/html,x"] }),
        json!({ "redirect_uris": [42] }),
        json!({ "redirect_uris": [format!("https://x.com/{}", "a".repeat(2048))] }),
    ] {
        let reply = harness.register(metadata.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "accepted {metadata}");
        assert_eq!(reply.error(), "invalid_client_metadata");
    }
    assert!(harness.auth.state.lock().clients.is_empty());
}

#[tokio::test]
async fn registration_evicts_the_oldest_idle_client_at_capacity() {
    let harness = Harness::new();
    let mut ids = Vec::new();
    for _ in 0..MAX_CLIENTS {
        ids.push(harness.register_client().await.id);
    }
    // Registrations within one millisecond tie, so pin the order explicitly.
    {
        let mut state = harness.auth.state.lock();
        for (i, id) in ids.iter().enumerate() {
            state.clients.get_mut(id).unwrap().created_at = Some(1_000 + i as i64);
        }
        // The very oldest client holds a live token, so it must survive.
        let record = TokenRecord {
            access_token: "a".into(),
            refresh_token: "r".into(),
            client_id: ids[0].clone(),
            expires_at: now_ms() + HOUR_MS,
            refresh_expires_at: now_ms() + HOUR_MS,
        };
        state.tokens.insert("a".into(), record);
    }
    let newest = harness.register_client().await;
    let state = harness.auth.state.lock();
    assert_eq!(state.clients.len(), MAX_CLIENTS);
    assert!(state.clients.contains_key(&ids[0]), "client with a live token was evicted");
    assert!(!state.clients.contains_key(&ids[1]), "oldest idle client should have been evicted");
    assert!(state.clients.contains_key(&newest.id));
}

#[tokio::test]
async fn registration_is_refused_when_every_client_holds_live_tokens() {
    let harness = Harness::new();
    {
        let mut state = harness.auth.state.lock();
        for i in 0..MAX_CLIENTS {
            let id = format!("client-{i}");
            let client = RegisteredClient {
                client_id: id.clone(),
                client_secret: Some("s".into()),
                token_endpoint_auth_method: AuthMethod::ClientSecretPost,
                redirect_uris: vec![REDIRECT_URI.into()],
                client_name: None,
                created_at: Some(i as i64),
            };
            state.clients.insert(id.clone(), client);
            let record = TokenRecord {
                access_token: format!("a{i}"),
                refresh_token: format!("r{i}"),
                client_id: id,
                expires_at: now_ms() + HOUR_MS,
                refresh_expires_at: now_ms() + HOUR_MS,
            };
            state.refresh_tokens.insert(format!("r{i}"), record);
        }
    }
    let reply = harness.register(json!({ "redirect_uris": [REDIRECT_URI] })).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(reply.error(), "too_many_clients");
    assert_eq!(harness.auth.state.lock().clients.len(), MAX_CLIENTS);
}

// --- /oauth/authorize ---

#[tokio::test]
async fn authorize_rejects_an_unknown_client() {
    let reply = Harness::new().authorize_page_for("unknown", &pkce().challenge, "https://x.com/cb").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Unknown client"), "body: {}", reply.body);
}

#[tokio::test]
async fn authorize_rejects_an_unregistered_redirect_uri() {
    let harness = Harness::new();
    let client = harness.register_client_for("https://legit.com/cb").await;
    let reply = harness.authorize_page_for(&client.id, &pkce().challenge, "https://evil.com/steal").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Invalid redirect URI"), "body: {}", reply.body);
}

#[tokio::test]
async fn authorize_requires_an_s256_pkce_challenge() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let base = format!("/oauth/authorize?client_id={}&redirect_uri={}", client.id, encode(REDIRECT_URI));
    let missing = harness.get(&format!("{base}&code_challenge_method=S256")).await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert!(missing.body.contains("PKCE"), "body: {}", missing.body);
    let plain = harness.get(&format!("{base}&code_challenge=test&code_challenge_method=plain")).await;
    assert_eq!(plain.status, StatusCode::BAD_REQUEST);
    assert!(harness.auth.state.lock().pending.is_empty());
}

fn encode(value: &str) -> String {
    crate::util::encode_uri_component(value)
}

#[tokio::test]
async fn authorize_renders_a_form_with_code_and_csrf() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let reply = harness.authorize_page(&client.id, &pkce().challenge).await;
    assert_eq!(reply.status, StatusCode::OK);
    let fields = reply.hidden_fields();
    assert!(fields.get("code").is_some_and(|c| !c.is_empty()), "no code field");
    assert!(fields.get("csrf").is_some_and(|c| !c.is_empty()), "no csrf field");
}

#[tokio::test]
async fn authorize_caps_pending_authorizations_until_they_expire() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    for _ in 0..MAX_PENDING {
        assert_eq!(harness.authorize_page(&client.id, "x").await.status, StatusCode::OK);
    }
    let full = harness.authorize_page(&client.id, "x").await;
    assert_eq!(full.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(full.body.contains("Too many pending authorizations"));
    harness.auth.advance_clock(PENDING_TTL_MS + 1);
    assert_eq!(harness.authorize_page(&client.id, "x").await.status, StatusCode::OK);
    assert_eq!(harness.auth.state.lock().pending.len(), 1, "expired requests are swept");
}

// Phishing defense (GHSA-49hr-4pv9-75q6): the user must see where the code goes.
#[tokio::test]
async fn consent_page_shows_the_redirect_host() {
    let harness = Harness::new();
    let client = harness.register_client_for("https://evil.example:8443/cb").await;
    let reply = harness.authorize_page_for(&client.id, &pkce().challenge, "https://evil.example:8443/cb").await;
    assert!(reply.body.contains("<b>evil.example:8443</b>"), "consent page must display the redirect host");
}

#[tokio::test]
async fn consent_page_escapes_the_self_reported_client_name() {
    let harness = Harness::new();
    let reply = harness.register(json!({ "client_name": "<script>x</script>", "redirect_uris": [REDIRECT_URI] })).await;
    let client_id = reply.json()["client_id"].as_str().unwrap().to_owned();
    let page = harness.authorize_page(&client_id, &pkce().challenge).await;
    assert!(page.body.contains("&lt;script&gt;x&lt;/script&gt;"));
    assert!(!page.body.contains("<script>x"));
}

// --- /oauth/approve ---

#[tokio::test]
async fn approve_rejects_an_unknown_code() {
    let reply = Harness::new().submit_password("bad-code", "bad-csrf", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn approve_rejects_a_wrong_csrf_token() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let fields = harness.authorize_page(&client.id, &pkce().challenge).await.hidden_fields();
    let reply = harness.submit_password(&fields["code"], "wrong-csrf", PASSWORD).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn approve_rejects_a_wrong_password_and_rotates_the_csrf_token() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let fields = harness.authorize_page(&client.id, &pkce().challenge).await.hidden_fields();
    let reply = harness.submit_password(&fields["code"], &fields["csrf"], "wrong").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.body.contains("Wrong password"));
    assert!(reply.body.contains(r#"aria-invalid="true""#));
    let retry = reply.hidden_fields();
    assert_eq!(retry["code"], fields["code"]);
    assert_ne!(retry["csrf"], fields["csrf"], "a failed attempt must rotate the CSRF token");
    let stale = harness.submit_password(&fields["code"], &fields["csrf"], PASSWORD).await;
    assert_eq!(stale.status, StatusCode::FORBIDDEN, "the old CSRF token must stop working");
    let ok = harness.submit_password(&retry["code"], &retry["csrf"], PASSWORD).await;
    assert_eq!(ok.status, StatusCode::FOUND);
}

#[tokio::test]
async fn approve_rejects_a_same_length_non_ascii_password() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let reply = harness.attempt_password(&client, "test-passworé").await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.body.contains("Wrong password"));
}

#[tokio::test]
async fn approve_treats_a_missing_password_as_wrong() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let fields = harness.authorize_page(&client.id, &pkce().challenge).await.hidden_fields();
    let reply =
        harness.post_form("/oauth/approve", &[("code", &fields["code"]), ("csrf", &fields["csrf"])], None).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn approve_redirects_with_code_and_state() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let reply = harness.attempt_password(&client, PASSWORD).await;
    assert_eq!(reply.status, StatusCode::FOUND);
    let location = reply.location();
    assert!(location.as_str().starts_with(REDIRECT_URI), "location: {location}");
    assert!(query_param(&location, "code").is_some_and(|c| !c.is_empty()));
    assert_eq!(query_param(&location, "state").as_deref(), Some("test-state"));
}

#[tokio::test]
async fn approve_keeps_query_parameters_of_the_redirect_uri() {
    let harness = Harness::new();
    let redirect_uri = "https://app.example.com/callback?tenant=a&code=stale";
    let client = harness.register_client_for(redirect_uri).await;
    let fields = harness.authorize_page_for(&client.id, &pkce().challenge, redirect_uri).await.hidden_fields();
    let reply = harness.submit_password(&fields["code"], &fields["csrf"], PASSWORD).await;
    let location = reply.location();
    assert_eq!(query_param(&location, "tenant").as_deref(), Some("a"));
    let codes: Vec<_> = location.query_pairs().filter(|(k, _)| k == "code").map(|(_, v)| v.into_owned()).collect();
    assert_eq!(codes, vec![fields["code"].clone()], "exactly one code, the issued one");
}

#[tokio::test]
async fn locks_out_after_five_failed_attempts() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    for attempt in 1..=5 {
        let reply = harness.attempt_password(&client, "wrong").await;
        if attempt < 5 {
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "attempt {attempt} should be 401");
        } else {
            assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS, "attempt {attempt} should lock out");
            assert!(reply.body.contains("Too many attempts. Try again in 5 seconds."), "body: {}", reply.body);
        }
    }
}

#[tokio::test]
async fn successful_login_resets_the_failure_counter() {
    let harness = Harness::with("mypass", None);
    let client = harness.register_client().await;
    for _ in 0..4 {
        harness.attempt_password(&client, "wrong").await;
    }
    assert_eq!(harness.attempt_password(&client, "mypass").await.status, StatusCode::FOUND);
    for attempt in 1..=4 {
        let reply = harness.attempt_password(&client, "wrong").await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "post-reset attempt {attempt} should be 401, not 429");
    }
}

#[tokio::test]
async fn lockout_blocks_even_the_right_password_and_backs_off_exponentially() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    for _ in 0..5 {
        harness.attempt_password(&client, "wrong").await;
    }
    let locked = harness.attempt_password(&client, PASSWORD).await;
    assert_eq!(locked.status, StatusCode::TOO_MANY_REQUESTS, "the right password must wait out the lockout");

    // Failures never reset until a success, so one more miss doubles the lockout.
    harness.auth.advance_clock(5_001);
    let second = harness.attempt_password(&client, "wrong").await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(second.body.contains("Try again in 10 seconds."), "body: {}", second.body);

    harness.auth.advance_clock(4_000);
    let waiting = harness.attempt_password(&client, PASSWORD).await;
    assert!(waiting.body.contains("Try again in 6 seconds."), "body: {}", waiting.body);

    harness.auth.advance_clock(6_001);
    assert_eq!(harness.attempt_password(&client, PASSWORD).await.status, StatusCode::FOUND);
    assert_eq!(harness.attempt_password(&client, "wrong").await.status, StatusCode::UNAUTHORIZED);
}

// --- Token exchange ---

#[tokio::test]
async fn code_exchange_requires_the_secret_without_consuming_the_code() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let base = [
        ("grant_type", "authorization_code"),
        ("code", approved.code.as_str()),
        ("client_id", approved.client.id.as_str()),
        ("code_verifier", approved.pkce.verifier.as_str()),
        ("redirect_uri", REDIRECT_URI),
    ];
    for secret in [None, Some("wrong-secret")] {
        let mut fields = base.to_vec();
        fields.extend(secret.map(|s| ("client_secret", s)));
        let reply = harness.token(&fields, None).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "secret {secret:?}");
        assert_eq!(reply.error(), "invalid_client");
    }
    let reply = harness.exchange(&approved, Some(&approved.pkce.verifier)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.json()["access_token"].is_string());
}

#[tokio::test]
async fn issues_tokens_for_a_correct_pkce_verifier() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let body = harness.exchange(&approved, Some(&approved.pkce.verifier)).await.json();
    assert!(body["access_token"].is_string());
    assert!(body["refresh_token"].is_string());
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], 3600);
}

#[tokio::test]
async fn rejects_an_incorrect_pkce_verifier() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let reply = harness.exchange(&approved, Some("wrong-verifier")).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "invalid_grant");
}

#[tokio::test]
async fn missing_code_verifier_is_invalid_grant() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let reply = harness.exchange(&approved, None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "invalid_grant");
}

#[tokio::test]
async fn rejects_a_different_client_id_at_exchange() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let reply = harness
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &approved.code),
                ("client_id", "wrong-client-id"),
                ("code_verifier", &approved.pkce.verifier),
                ("redirect_uri", REDIRECT_URI),
            ],
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "invalid_grant");
}

#[tokio::test]
async fn authorization_code_is_single_use() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    assert_eq!(harness.exchange(&approved, Some(&approved.pkce.verifier)).await.status, StatusCode::OK);
    let again = harness.exchange(&approved, Some(&approved.pkce.verifier)).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(again.error(), "invalid_grant");
}

#[tokio::test]
async fn a_failed_pkce_or_redirect_check_consumes_the_code() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    assert_eq!(harness.exchange(&approved, Some("wrong-verifier")).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(harness.exchange(&approved, Some(&approved.pkce.verifier)).await.status, StatusCode::BAD_REQUEST);

    let approved = harness.approved_code().await;
    let secret = approved.client.secret.clone().unwrap();
    let wrong_redirect = harness
        .token(
            &[
                ("grant_type", "authorization_code"),
                ("code", &approved.code),
                ("client_id", &approved.client.id),
                ("client_secret", &secret),
                ("code_verifier", &approved.pkce.verifier),
                ("redirect_uri", "https://app.example.com/other"),
            ],
            None,
        )
        .await;
    assert_eq!(wrong_redirect.status, StatusCode::BAD_REQUEST);
    assert_eq!(wrong_redirect.json()["error_description"], "redirect_uri mismatch");
    assert_eq!(harness.exchange(&approved, Some(&approved.pkce.verifier)).await.status, StatusCode::BAD_REQUEST);
}

// GHSA-cc9w-6w4g-hqv7: the code is in the authorize page HTML before any password.
#[tokio::test]
async fn rejects_the_authorize_page_code_when_the_password_step_was_skipped() {
    let harness = Harness::new();
    let client = harness.register_client().await;
    let pkce = pkce();
    let fields = harness.authorize_page(&client.id, &pkce.challenge).await.hidden_fields();
    let skipped = Approved { client, pkce, code: fields["code"].clone() };
    let reply = harness.exchange(&skipped, Some(&skipped.pkce.verifier)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    let body = reply.json();
    assert_eq!(body["error"], "invalid_grant");
    assert!(body.get("access_token").is_none());
    assert!(harness.auth.state.lock().tokens.is_empty(), "no token may be minted");
}

#[tokio::test]
async fn rejects_an_expired_authorization_code() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    harness.auth.advance_clock(PENDING_TTL_MS + 1);
    let reply = harness.exchange(&approved, Some(&approved.pkce.verifier)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "invalid_grant");
    assert!(harness.auth.state.lock().pending.is_empty(), "the expired code should be dropped");
}

#[tokio::test]
async fn rejects_an_unsupported_grant_type() {
    let reply = Harness::new().token(&[("grant_type", "client_credentials")], None).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error(), "unsupported_grant_type");
}

#[tokio::test]
async fn token_endpoint_accepts_a_json_body() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let body = json!({
        "grant_type": "authorization_code",
        "code": approved.code,
        "client_id": approved.client.id,
        "client_secret": approved.client.secret,
        "code_verifier": approved.pkce.verifier,
        "redirect_uri": REDIRECT_URI,
    });
    let reply = harness.post_json("/oauth/token", &body.to_string()).await;
    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.body);
}

// --- Client authentication via HTTP Basic (client_secret_basic) ---

fn code_fields<'a>(approved: &'a Approved, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("code", approved.code.as_str()),
        ("code_verifier", approved.pkce.verifier.as_str()),
        ("redirect_uri", REDIRECT_URI),
    ];
    fields.extend_from_slice(extra);
    fields
}

#[tokio::test]
async fn basic_credentials_work_for_exchange_and_refresh() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let authorization = basic(&approved.client.id, approved.client.secret.as_deref().unwrap());

    let exchange = harness.token(&code_fields(&approved, &[]), Some(&authorization)).await;
    assert_eq!(exchange.status, StatusCode::OK, "body: {}", exchange.body);
    let tokens = exchange.json();
    assert!(harness.is_valid(tokens["access_token"].as_str().unwrap()));

    let refresh_token = tokens["refresh_token"].as_str().unwrap();
    let refresh =
        harness.token(&[("grant_type", "refresh_token"), ("refresh_token", refresh_token)], Some(&authorization)).await;
    assert_eq!(refresh.status, StatusCode::OK, "body: {}", refresh.body);
    assert!(harness.is_valid(refresh.json()["access_token"].as_str().unwrap()));
}

#[tokio::test]
async fn a_wrong_basic_secret_does_not_consume_the_code() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let wrong = harness.token(&code_fields(&approved, &[]), Some(&basic(&approved.client.id, "wrong-secret"))).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.error(), "invalid_client");
    let right = basic(&approved.client.id, approved.client.secret.as_deref().unwrap());
    assert_eq!(harness.token(&code_fields(&approved, &[]), Some(&right)).await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_basic_secret_is_rejected_on_refresh() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    let reply = harness
        .token(
            &[("grant_type", "refresh_token"), ("refresh_token", &tokens.refresh_token)],
            Some(&basic(&tokens.client.id, "wrong-secret")),
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error(), "invalid_client");
    assert!(harness.is_valid(&tokens.access_token), "a rejected refresh must not revoke the session");
}

#[tokio::test]
async fn basic_credentials_that_disagree_with_the_body_are_rejected() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let secret = approved.client.secret.clone().unwrap();
    let authorization = basic(&approved.client.id, &secret);
    for extra in [[("client_id", "other-client")], [("client_secret", "other-secret")]] {
        let reply = harness.token(&code_fields(&approved, &extra), Some(&authorization)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{extra:?}");
        assert_eq!(reply.error(), "invalid_client");
    }
    let agreeing = [("client_id", approved.client.id.as_str()), ("client_secret", secret.as_str())];
    let reply = harness.token(&code_fields(&approved, &agreeing), Some(&authorization)).await;
    assert_eq!(reply.status, StatusCode::OK, "agreeing body credentials alongside Basic are fine");
}

#[tokio::test]
async fn a_malformed_basic_header_is_rejected() {
    let harness = Harness::new();
    let approved = harness.approved_code().await;
    let reply = harness.token(&code_fields(&approved, &[]), Some("Basic !!!not-base64-without-colon")).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.error(), "invalid_client");
}

#[test]
fn basic_credentials_are_form_decoded() {
    let empty = HashMap::new();
    let header = format!("basic {}", URL_SAFE_NO_PAD.encode("a%3Ab:s%20+x"));
    // URL-safe and standard alphabets agree on this input, and padding is optional.
    let credentials = client_credentials(Some(&header), &empty).expect("valid Basic header");
    assert_eq!(credentials.client_id.as_deref(), Some("a:b"));
    assert_eq!(credentials.secret.as_deref(), Some("s  x"));

    let no_colon = format!("Basic {}", STANDARD.encode("no-colon"));
    assert!(client_credentials(Some(&no_colon), &empty).is_none());
    let bad_escape = format!("Basic {}", STANDARD.encode("id:%zz"));
    assert!(client_credentials(Some(&bad_escape), &empty).is_none());

    // A bearer header is not client authentication; the body speaks.
    let body = HashMap::from([("client_id".to_owned(), "c".to_owned())]);
    let credentials = client_credentials(Some("Bearer abc"), &body).unwrap();
    assert_eq!(credentials.client_id.as_deref(), Some("c"));
    assert_eq!(credentials.secret, None);
}

// --- Refresh ---

#[tokio::test]
async fn public_clients_exchange_and_refresh_without_a_secret() {
    let harness = Harness::new();
    let client = harness.register_public_client().await;
    let approved = harness.approved_code_for(client).await;
    let exchange = harness.exchange(&approved, Some(&approved.pkce.verifier)).await;
    assert_eq!(exchange.status, StatusCode::OK, "body: {}", exchange.body);
    let refresh_token = exchange.json()["refresh_token"].as_str().unwrap().to_owned();
    let refresh = harness.refresh(&refresh_token, &approved.client).await;
    assert_eq!(refresh.status, StatusCode::OK, "body: {}", refresh.body);
    assert!(harness.is_valid(refresh.json()["access_token"].as_str().unwrap()));
}

#[tokio::test]
async fn refresh_requires_the_issuing_clients_credentials_without_consuming_the_token() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    let other = harness.register_client().await;
    let other_secret = other.secret.clone().unwrap();
    let cases: [(&[Field], StatusCode, &str); 4] = [
        (&[("client_id", &tokens.client.id)], StatusCode::UNAUTHORIZED, "invalid_client"),
        (
            &[("client_id", &tokens.client.id), ("client_secret", "wrong-secret")],
            StatusCode::UNAUTHORIZED,
            "invalid_client",
        ),
        (&[("client_id", &other.id), ("client_secret", &other_secret)], StatusCode::BAD_REQUEST, "invalid_grant"),
        (&[], StatusCode::UNAUTHORIZED, "invalid_client"),
    ];
    for (credentials, status, error) in cases {
        let mut fields = vec![("grant_type", "refresh_token"), ("refresh_token", tokens.refresh_token.as_str())];
        fields.extend_from_slice(credentials);
        let reply = harness.token(&fields, None).await;
        assert_eq!(reply.status, status, "credentials {credentials:?}");
        let body = reply.json();
        assert_eq!(body["error"], error, "credentials {credentials:?}");
        assert!(body.get("access_token").is_none());
        assert!(harness.is_valid(&tokens.access_token), "a rejected refresh must not revoke the session");
    }
    let reply = harness.refresh(&tokens.refresh_token, &tokens.client).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(harness.is_valid(reply.json()["access_token"].as_str().unwrap()));
}

#[tokio::test]
async fn refresh_rotates_both_tokens() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    assert!(harness.is_valid(&tokens.access_token));

    let reply = harness.refresh(&tokens.refresh_token, &tokens.client).await;
    assert_eq!(reply.status, StatusCode::OK);
    let rotated = reply.json();
    let new_access = rotated["access_token"].as_str().unwrap();
    assert_ne!(new_access, tokens.access_token);
    assert_ne!(rotated["refresh_token"].as_str().unwrap(), tokens.refresh_token);
    assert!(!harness.is_valid(&tokens.access_token), "the old access token must stop working");
    assert!(harness.is_valid(new_access));

    let replay = harness.refresh(&tokens.refresh_token, &tokens.client).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST, "the old refresh token must stop working");
}

#[tokio::test]
async fn rotation_keeps_the_original_refresh_expiry() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    let original = harness.auth.state.lock().refresh_tokens[&tokens.refresh_token].refresh_expires_at;
    assert!((original - now_ms() - 14 * DAY_MS).abs() < 60_000, "refresh lifetime should be MCP_REFRESH_DAYS");
    harness.auth.advance_clock(DAY_MS);
    let rotated = harness.refresh(&tokens.refresh_token, &tokens.client).await.json();
    let state = harness.auth.state.lock();
    let record = &state.refresh_tokens[rotated["refresh_token"].as_str().unwrap()];
    assert_eq!(record.refresh_expires_at, original - DAY_MS, "rotation must not extend the session");
}

#[tokio::test]
async fn an_expired_refresh_token_is_rejected_and_dropped() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    harness.auth.advance_clock(14 * DAY_MS + 1);
    let reply = harness.refresh(&tokens.refresh_token, &tokens.client).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error_description"], "Refresh token expired");
    assert!(harness.auth.state.lock().refresh_tokens.is_empty());
}

// --- validate_token ---

#[tokio::test]
async fn validate_token_rejects_missing_foreign_and_unknown_headers() {
    let harness = Harness::new();
    assert!(!harness.auth.validate_token(None));
    assert!(!harness.auth.validate_token(Some("Basic abc")));
    assert!(!harness.auth.validate_token(Some("Bearer bad-token")));
    assert!(!harness.auth.validate_token(Some("Bearer")));
}

#[tokio::test]
async fn validate_token_accepts_a_live_token_with_any_scheme_case() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    assert!(harness.is_valid(&tokens.access_token));
    assert!(harness.auth.validate_token(Some(&format!("bearer {}", tokens.access_token))));
    assert!(harness.auth.validate_token(Some(&format!("BEARER  {}", tokens.access_token))));
}

#[tokio::test]
async fn access_tokens_expire_after_an_hour() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    harness.auth.advance_clock(HOUR_MS - 60_000);
    assert!(harness.is_valid(&tokens.access_token));
    harness.auth.advance_clock(60_001);
    assert!(!harness.is_valid(&tokens.access_token));
    assert!(harness.auth.state.lock().tokens.is_empty(), "an expired token is dropped when seen");
    // The refresh token outlives the access token.
    assert_eq!(harness.refresh(&tokens.refresh_token, &tokens.client).await.status, StatusCode::OK);
}

#[tokio::test]
async fn cleanup_drops_expired_state_but_keeps_clients() {
    let harness = Harness::new();
    let tokens = harness.complete_flow().await;
    let client = harness.register_client().await;
    harness.authorize_page(&client.id, &pkce().challenge).await;
    harness.auth.advance_clock(HOUR_MS + 1);
    harness.auth.cleanup();
    {
        let state = harness.auth.state.lock();
        assert!(state.tokens.is_empty());
        assert!(state.pending.is_empty() && state.csrf.is_empty());
        assert_eq!(state.refresh_tokens.len(), 1, "refresh tokens live for days");
        assert_eq!(state.clients.len(), 2, "registrations are never expired");
    }
    assert!(!harness.is_valid(&tokens.access_token));
}

// --- safe_equal ---

#[test]
fn safe_equal_matches_identical_strings_only() {
    assert!(safe_equal("Bearer abc", "Bearer abc"));
    assert!(!safe_equal("Bearer abc", "Bearer abd"));
    assert!(!safe_equal("Bearer abc", "Bearer abcd"));
    assert!(!safe_equal("", "Bearer abc"));
    // Same UTF-16 length, different UTF-8 length.
    assert!(!safe_equal("Bearer abcé", "Bearer abcd"));
}

// --- Persistence ---

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn persisted_state_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/auth-tokens.json");
    let first = Harness::persisted(&path);
    let tokens = first.complete_flow().await;

    let on_disk = read_json(&path);
    assert_eq!(on_disk["tokens"][&tokens.access_token]["clientId"], tokens.client.id.as_str());
    assert_eq!(on_disk["refreshTokens"][&tokens.refresh_token]["accessToken"], tokens.access_token.as_str());
    let client = &on_disk["clients"][&tokens.client.id];
    assert_eq!(client["tokenEndpointAuthMethod"], "client_secret_post");
    assert_eq!(client["redirectUris"], json!([REDIRECT_URI]));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be private, got {mode:o}");
    }
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
    assert_eq!(leftovers.len(), 1, "no temporary files may be left behind");

    let second = Harness::persisted(&path);
    assert!(second.auth.load_tokens().await, "a live session was restored");
    assert!(second.is_valid(&tokens.access_token));
    assert_eq!(second.refresh(&tokens.refresh_token, &tokens.client).await.status, StatusCode::OK);
}

#[tokio::test]
async fn client_registrations_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-tokens.json");
    let client = Harness::persisted(&path).register_client().await;
    let restarted = Harness::persisted(&path);
    assert!(!restarted.auth.load_tokens().await, "no sessions yet, only a registration");
    let reply = restarted.authorize_page(&client.id, "x").await;
    assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.body);
}

#[tokio::test]
async fn saving_drops_expired_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-tokens.json");
    let harness = Harness::persisted(&path);
    harness.complete_flow().await;
    harness.auth.advance_clock(HOUR_MS + 1);
    harness.auth.save_tokens().await;
    let on_disk = read_json(&path);
    assert_eq!(on_disk["tokens"], json!({}));
    assert_eq!(on_disk["refreshTokens"].as_object().unwrap().len(), 1);
    assert_eq!(on_disk["clients"].as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn loading_skips_expired_and_malformed_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-tokens.json");
    let now = now_ms();
    let record = |access: &str, expires_at: i64, refresh_expires_at: i64| {
        json!({
            "accessToken": access,
            "refreshToken": format!("{access}-refresh"),
            "clientId": "c",
            "expiresAt": expires_at,
            "refreshExpiresAt": refresh_expires_at,
        })
    };
    let data = json!({
        "tokens": {
            "live": record("live", now + 60_000, now + DAY_MS),
            "stale": record("stale", now - 1, now + DAY_MS),
            "broken": { "accessToken": "broken" },
        },
        "refreshTokens": {
            "live-refresh": record("live", now + 60_000, now + DAY_MS),
            "stale-refresh": record("stale", now - 1, now - 1),
        },
        "clients": { "c": { "clientId": "c", "redirectUris": [REDIRECT_URI] }, "bad": { "clientId": 1 } },
    });
    std::fs::write(&path, data.to_string()).unwrap();
    let harness = Harness::persisted(&path);
    assert!(harness.auth.load_tokens().await);
    let state = harness.auth.state.lock();
    assert_eq!(state.tokens.keys().collect::<Vec<_>>(), vec!["live"]);
    assert_eq!(state.refresh_tokens.keys().collect::<Vec<_>>(), vec!["live-refresh"]);
    assert_eq!(state.clients.keys().collect::<Vec<_>>(), vec!["c"]);
}

#[tokio::test]
async fn loading_a_missing_or_corrupt_file_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-tokens.json");
    assert!(!Harness::persisted(&path).auth.load_tokens().await);
    std::fs::write(&path, "{truncated").unwrap();
    let harness = Harness::persisted(&path);
    assert!(!harness.auth.load_tokens().await);
    assert!(harness.auth.state.lock().clients.is_empty());
}

#[tokio::test]
async fn legacy_clients_without_an_auth_method_must_authenticate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth-tokens.json");
    let now = now_ms();
    let data = json!({
        "clients": {
            "legacy": {
                "clientId": "legacy",
                "clientSecret": "legacy-secret",
                "redirectUris": [REDIRECT_URI],
            },
        },
        "refreshTokens": {
            "refresh": {
                "clientId": "legacy",
                "accessToken": "old-access",
                "refreshToken": "refresh",
                "expiresAt": now + 60_000,
                "refreshExpiresAt": now + 60_000,
            },
        },
    });
    std::fs::write(&path, data.to_string()).unwrap();
    let harness = Harness::persisted(&path);
    harness.auth.load_tokens().await;
    for (secret, status) in [("wrong", StatusCode::UNAUTHORIZED), ("legacy-secret", StatusCode::OK)] {
        let client = Client { id: "legacy".into(), secret: Some(secret.into()) };
        let reply = harness.refresh("refresh", &client).await;
        assert_eq!(reply.status, status, "secret {secret:?}: {}", reply.body);
    }
}

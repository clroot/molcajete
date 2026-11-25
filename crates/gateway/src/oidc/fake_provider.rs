//! An OpenID Connect provider for tests: discovery, JWKS, a token endpoint
//! that checks PKCE and hands out RS256-signed ID tokens, and an end session
//! endpoint. The authorization step is skipped: tests call
//! [`FakeProvider::authorize`] with the gateway's authorization URL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use openidconnect::core::{CoreJsonWebKeySet, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey};
use openidconnect::{JsonWebKeyId, PrivateSigningKey as _};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub const CLIENT_ID: &str = "molcajete";
pub const CLIENT_SECRET: &str = "client-secret";
const KEY_PEM: &str = include_str!("test-signing-key.pem");

/// Who signs in next and how the ID token is made.
#[derive(Clone)]
pub struct Login {
    pub subject: String,
    pub username: String,
    pub groups: Vec<String>,
    /// `aud` of the ID token; the gateway's client id unless changed.
    pub audience: String,
    /// Replaces the nonce the gateway sent.
    pub nonce: Option<String>,
}

impl Login {
    pub fn new(subject: &str, username: &str, groups: &[&str]) -> Self {
        Self {
            subject: subject.into(),
            username: username.into(),
            groups: groups.iter().map(|g| g.to_string()).collect(),
            audience: CLIENT_ID.into(),
            nonce: None,
        }
    }
}

struct Grant {
    login: Login,
    nonce: String,
    challenge: String,
    redirect_uri: String,
}

struct Inner {
    issuer: String,
    key: CoreRsaPrivateSigningKey,
    grants: Mutex<HashMap<String, Grant>>,
}

#[derive(Clone)]
pub struct FakeProvider(Arc<Inner>);

impl FakeProvider {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let key = CoreRsaPrivateSigningKey::from_pem(KEY_PEM, Some(JsonWebKeyId::new("k1".into())))
            .unwrap();
        let provider = Self(Arc::new(Inner {
            issuer,
            key,
            grants: Default::default(),
        }));
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(provider.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        provider
    }

    pub fn issuer(&self) -> &str {
        &self.0.issuer
    }

    /// What the provider does when the browser arrives at `authorize_url`
    /// and `login` signs in: the `code` and `state` for the callback.
    pub fn authorize(&self, authorize_url: &str, login: Login) -> (String, String) {
        let url = openidconnect::url::Url::parse(authorize_url).unwrap();
        assert!(authorize_url.starts_with(&format!("{}/authorize?", self.0.issuer)));
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], CLIENT_ID);
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(params["scope"].split(' ').any(|s| s == "openid"));
        let code = crate::auth::new_token();
        self.0.grants.lock().unwrap().insert(
            code.clone(),
            Grant {
                login,
                nonce: params["nonce"].clone(),
                challenge: params["code_challenge"].clone(),
                redirect_uri: params["redirect_uri"].clone(),
            },
        );
        (code, params["state"].clone())
    }

    fn id_token(&self, grant: &Grant) -> String {
        let b64 = |v: &Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
        };
        let now = crate::db::now_millis() / 1000;
        let login = &grant.login;
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": "k1" });
        let claims = json!({
            "iss": self.0.issuer,
            "sub": login.subject,
            "aud": login.audience,
            "iat": now,
            "exp": now + 300,
            "nonce": login.nonce.clone().unwrap_or_else(|| grant.nonce.clone()),
            "preferred_username": login.username,
            "groups": login.groups,
        });
        let signing_input = format!("{}.{}", b64(&header), b64(&claims));
        let signature = self
            .0
            .key
            .sign(
                &CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
                signing_input.as_bytes(),
            )
            .unwrap();
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature);
        format!("{signing_input}.{signature}")
    }
}

async fn discovery(State(p): State<FakeProvider>) -> Json<Value> {
    let issuer = &p.0.issuer;
    Json(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "end_session_endpoint": format!("{issuer}/logout"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

async fn jwks(State(p): State<FakeProvider>) -> Json<CoreJsonWebKeySet> {
    Json(CoreJsonWebKeySet::new(vec![p.0.key.as_verification_key()]))
}

async fn token(
    State(p): State<FakeProvider>,
    headers: axum::http::HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let refuse =
        |error: &str| (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
    let basic = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(basic.as_str()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return refuse("unsupported_grant_type");
    }
    // Codes are used once.
    let grant = form
        .get("code")
        .and_then(|code| p.0.grants.lock().unwrap().remove(code));
    let Some(grant) = grant else {
        return refuse("invalid_grant");
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    if challenge != grant.challenge || form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return refuse("invalid_grant");
    }
    Json(json!({
        "access_token": crate::auth::new_token(),
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": p.id_token(&grant),
    }))
    .into_response()
}

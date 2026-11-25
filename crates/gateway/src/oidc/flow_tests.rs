//! The sign-in flow end to end, against [`FakeProvider`].

use axum::body::Body;
use axum::http::{Method, Request};
use tower::ServiceExt as _;

use super::fake_provider::{CLIENT_ID, CLIENT_SECRET, FakeProvider, Login};
use super::*;
use crate::config::Config;
use crate::{bootstrap, db, router};

struct Sso {
    router: axum::Router,
    state: Arc<AppState>,
    provider: FakeProvider,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// What came back from the callback.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The `Cookie` header value of the new session.
    SignedIn(String),
    /// `?login_error=<code>`.
    Refused(String),
}

impl Outcome {
    fn session(self) -> String {
        match self {
            Self::SignedIn(cookie) => cookie,
            Self::Refused(code) => panic!("sign-in refused: {code}"),
        }
    }
}

/// A gateway with an `admin` administrator (password `admin-password`) and
/// single sign-on through a fake provider; `extra` goes into `[oidc]`.
async fn sso(extra: &str) -> Sso {
    let provider = FakeProvider::start().await;
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<!doctype html>").unwrap();
    let toml = format!(
        "[server]\nstatic_dir = {static_dir:?}\n\
         [[users]]\nusername = \"admin\"\npassword = \"admin-password\"\n\
         [oidc]\nissuer = {issuer:?}\nclient_id = {CLIENT_ID:?}\n\
         client_secret = {CLIENT_SECRET:?}\n\
         redirect_url = \"http://gw.test/api/auth/oidc/callback\"\n\
         post_logout_redirect_url = \"http://gw.test/\"\n\
         admin_groups = [\"molcajete-admins\"]\n{extra}\n",
        static_dir = web.path().display().to_string(),
        issuer = provider.issuer(),
    );
    let config: Config = ::toml::from_str(&toml).unwrap();
    let (db, data) = db::tests::sqlite().await;
    bootstrap::seed(&db, &config).await.unwrap();
    let oidc = Oidc::new(config.oidc.clone().unwrap()).unwrap();
    let state = Arc::new(AppState {
        config,
        db,
        client_reports: false,
        tunnels: Default::default(),
        oidc: Some(oidc),
    });
    Sso {
        router: router(state.clone()),
        state,
        provider,
        _dirs: (web, data),
    }
}

fn set_cookies(response: &Response) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_owned())
        .collect()
}

fn location(response: &Response) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

async fn body_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

impl Sso {
    async fn send(&self, request: Request<Body>) -> Response {
        self.router.clone().oneshot(request).await.unwrap()
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::get(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    /// Starts a sign-in: the provider's authorization URL and the state
    /// cookie (`name=value`).
    async fn start(&self) -> (String, String) {
        let response = self.get("/api/auth/oidc/login", None).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookies = set_cookies(&response);
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].contains("HttpOnly") && cookies[0].contains("SameSite=Lax"));
        let cookie = cookies[0].split(';').next().unwrap().to_owned();
        (location(&response).to_owned(), cookie)
    }

    async fn callback(&self, query: &str, cookie: Option<&str>) -> Outcome {
        let response = self
            .get(&format!("/api/auth/oidc/callback?{query}"), cookie)
            .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookies = set_cookies(&response);
        // The state cookie is always cleared.
        assert!(cookies[0].starts_with(&format!("{STATE_COOKIE}=;")));
        match location(&response).strip_prefix("../../../") {
            Some("") => {
                assert!(cookies[1].contains("SameSite=Strict"));
                Outcome::SignedIn(cookies[1].split(';').next().unwrap().to_owned())
            }
            Some(rest) => Outcome::Refused(rest.strip_prefix("?login_error=").unwrap().to_owned()),
            None => panic!("unexpected location {}", location(&response)),
        }
    }

    async fn sign_in(&self, login: Login) -> Outcome {
        let (authorize_url, cookie) = self.start().await;
        let (code, state) = self.provider.authorize(&authorize_url, login);
        self.callback(&format!("code={code}&state={state}"), Some(&cookie))
            .await
    }

    async fn me(&self, session: &str) -> Value {
        let response = self.get("/api/auth/me", Some(session)).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    }

    async fn password_login(&self, username: &str, password: &str) -> StatusCode {
        let request = Request::post("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "username": username, "password": password }).to_string(),
            ))
            .unwrap();
        self.send(request).await.status()
    }
}

#[tokio::test]
async fn first_sign_in_creates_the_user() {
    let sso = sso("").await;
    let providers = body_json(sso.get("/api/auth/providers", None).await).await;
    assert_eq!(
        providers,
        serde_json::json!({ "passwordLogin": true, "oidc": { "displayName": "single sign-on" } })
    );

    let session = sso
        .sign_in(Login::new("sub-ada", "ada", &[]))
        .await
        .session();
    let me = sso.me(&session).await;
    assert_eq!(me["username"], "ada");
    assert_eq!(me["isAdmin"], false);
    assert!(me["lastLoginAt"].is_i64());

    // The same identity signs in to the same account; it has no password.
    let again = sso
        .sign_in(Login::new("sub-ada", "ada", &[]))
        .await
        .session();
    assert_eq!(sso.me(&again).await["id"], me["id"]);
    assert_eq!(sso.state.db.count_users().await.unwrap(), 2);
    assert_eq!(
        sso.password_login("ada", "").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        sso.password_login("ada", "!").await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn tampered_state_is_refused() {
    let sso = sso("").await;
    let (authorize_url, cookie) = sso.start().await;
    let (code, _) = sso
        .provider
        .authorize(&authorize_url, Login::new("s", "eve", &[]));
    let outcome = sso
        .callback(&format!("code={code}&state=forged"), Some(&cookie))
        .await;
    assert_eq!(outcome, Outcome::Refused("expired".into()));

    // A callback without the cookie of the browser that started it.
    let (authorize_url, _) = sso.start().await;
    let (code, state) = sso
        .provider
        .authorize(&authorize_url, Login::new("s", "eve", &[]));
    let outcome = sso
        .callback(&format!("code={code}&state={state}"), None)
        .await;
    assert_eq!(outcome, Outcome::Refused("expired".into()));
    assert!(matches!(
        sso.state.db.user_credentials("eve").await,
        Err(DbError::NotFound)
    ));
}

#[tokio::test]
async fn replayed_callback_is_refused() {
    let sso = sso("").await;
    let (authorize_url, cookie) = sso.start().await;
    let (code, state) = sso
        .provider
        .authorize(&authorize_url, Login::new("s", "ada", &[]));
    let query = format!("code={code}&state={state}");
    sso.callback(&query, Some(&cookie)).await.session();
    assert_eq!(
        sso.callback(&query, Some(&cookie)).await,
        Outcome::Refused("expired".into())
    );
}

#[tokio::test]
async fn tokens_for_another_client_or_nonce_are_refused() {
    let sso = sso("").await;
    let mut login = Login::new("s", "mallory", &[]);
    login.audience = "another-client".into();
    assert_eq!(sso.sign_in(login).await, Outcome::Refused("failed".into()));

    let mut login = Login::new("s", "mallory", &[]);
    login.nonce = Some("stale-nonce".into());
    assert_eq!(sso.sign_in(login).await, Outcome::Refused("failed".into()));
    assert!(sso.state.db.user_credentials("mallory").await.is_err());
}

#[tokio::test]
async fn provider_errors_are_reported() {
    let sso = sso("").await;
    let (_, cookie) = sso.start().await;
    let state = cookie.split_once('=').unwrap().1;
    assert_eq!(
        sso.callback(&format!("error=access_denied&state={state}"), Some(&cookie))
            .await,
        Outcome::Refused("cancelled".into())
    );
}

#[tokio::test]
async fn admin_groups_grant_and_take_away_admin() {
    let sso = sso("").await;
    let session = sso
        .sign_in(Login::new(
            "sub-root",
            "root",
            &["staff", "molcajete-admins"],
        ))
        .await
        .session();
    assert_eq!(sso.me(&session).await["isAdmin"], true);
    assert_eq!(
        sso.get("/api/users", Some(&session)).await.status(),
        StatusCode::OK
    );

    let session = sso
        .sign_in(Login::new("sub-root", "root", &["staff"]))
        .await
        .session();
    assert_eq!(sso.me(&session).await["isAdmin"], false);
}

#[tokio::test]
async fn admin_groups_only_grant_without_sync() {
    let sso = sso("sync_admin = false").await;
    let login = |groups: &[&str]| Login::new("sub-root", "root", groups);
    sso.sign_in(login(&["molcajete-admins"])).await.session();
    let session = sso.sign_in(login(&[])).await.session();
    assert_eq!(sso.me(&session).await["isAdmin"], true);
}

#[tokio::test]
async fn only_known_users_without_auto_create() {
    let sso = sso("auto_create_users = false").await;
    assert_eq!(
        sso.sign_in(Login::new("sub-x", "stranger", &[])).await,
        Outcome::Refused("not_allowed".into())
    );

    // An administrator creates the user without a password beforehand.
    let admin = {
        let request = Request::post("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"username":"admin","password":"admin-password"}"#,
            ))
            .unwrap();
        let response = sso.send(request).await;
        set_cookies(&response)[0]
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/users")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &admin)
        .body(Body::from(r#"{"username":"grace"}"#))
        .unwrap();
    assert_eq!(sso.send(request).await.status(), StatusCode::CREATED);
    let created = sso.state.db.user_credentials("grace").await.unwrap().0;

    // Matched by username the first time, by subject afterwards, so a
    // rename at the provider follows along.
    let session = sso
        .sign_in(Login::new("sub-grace", "grace", &[]))
        .await
        .session();
    assert_eq!(sso.me(&session).await["id"], created.id.as_str());
    let session = sso
        .sign_in(Login::new("sub-grace", "grace.hopper", &[]))
        .await
        .session();
    let me = sso.me(&session).await;
    assert_eq!(
        (me["id"].as_str(), me["username"].as_str()),
        (Some(created.id.as_str()), Some("grace.hopper"))
    );

    // Someone else at the provider cannot take over the linked account.
    assert_eq!(
        sso.sign_in(Login::new("sub-imposter", "grace.hopper", &[]))
            .await,
        Outcome::Refused("username_taken".into())
    );
}

#[tokio::test]
async fn disabled_users_cannot_sign_in() {
    let sso = sso("").await;
    let session = sso.sign_in(Login::new("sub-d", "dan", &[])).await.session();
    let id = sso.me(&session).await["id"].as_str().unwrap().to_owned();
    let update = UserUpdate {
        disabled: Some(true),
        ..Default::default()
    };
    sso.state.db.update_user(&id, update).await.unwrap();
    assert_eq!(
        sso.sign_in(Login::new("sub-d", "dan", &[])).await,
        Outcome::Refused("disabled".into())
    );
}

#[tokio::test]
async fn password_login_can_be_turned_off() {
    let sso = sso("password_login = false").await;
    let providers = body_json(sso.get("/api/auth/providers", None).await).await;
    assert_eq!(providers["passwordLogin"], false);
    assert_eq!(
        sso.password_login("admin", "admin-password").await,
        StatusCode::FORBIDDEN
    );
    sso.sign_in(Login::new("s", "ada", &[])).await.session();
}

#[tokio::test]
async fn logout_signs_out_at_the_provider() {
    let sso = sso("").await;
    let session = sso.sign_in(Login::new("sub-l", "lou", &[])).await.session();
    let request = Request::post("/api/auth/logout")
        .header(header::COOKIE, &session)
        .body(Body::empty())
        .unwrap();
    let response = sso.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let url = body_json(response).await["logoutUrl"]
        .as_str()
        .unwrap()
        .to_owned();
    let url = openidconnect::url::Url::parse(&url).unwrap();
    assert_eq!(
        url.as_str().split('?').next(),
        Some(&*format!("{}/logout", sso.provider.issuer()))
    );
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], CLIENT_ID);
    assert_eq!(params["post_logout_redirect_uri"], "http://gw.test/");
    assert!(params.contains_key("id_token_hint"));
    assert_eq!(
        sso.get("/api/auth/me", Some(&session)).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Password sessions sign out here only.
    let request = Request::post("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"username":"admin","password":"admin-password"}"#,
        ))
        .unwrap();
    let response = sso.send(request).await;
    let admin = set_cookies(&response)[0]
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let request = Request::post("/api/auth/logout")
        .header(header::COOKIE, &admin)
        .body(Body::empty())
        .unwrap();
    assert_eq!(sso.send(request).await.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unreachable_provider_is_reported() {
    let sso = sso("").await;
    let mut config = sso.state.config.oidc.clone().unwrap();
    // Nothing listens on port 9 here.
    config.issuer = "http://127.0.0.1:9".into();
    let state = Arc::new(AppState {
        config: sso.state.config.clone(),
        db: sso.state.db.clone(),
        client_reports: false,
        tunnels: Default::default(),
        oidc: Some(Oidc::new(config).unwrap()),
    });
    let response = router(state)
        .oneshot(
            Request::get("/api/auth/oidc/login")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(location(&response), "../../../?login_error=unavailable");
}

use super::*;

const SECRET: &[u8] = b"test-secret";
const HS256: &str = r#"{"alg":"HS256","typ":"JWT"}"#;
const ALICE: &str = r#"{"sub":"7","username":"alice","exp":4102444800}"#;
const SCHEMA: &str = "
    CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE);
    CREATE TABLE posts (id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL, body TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')));
    CREATE TABLE likes (user_id INTEGER NOT NULL, post_id INTEGER NOT NULL,
        PRIMARY KEY (user_id, post_id));
    INSERT INTO users VALUES (7, 'alice');";

async fn api() -> Api {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    Api {
        pool,
        mac: Hmac::new_from_slice(SECRET).unwrap(),
        started: Instant::now(),
    }
}

fn token(header: &str, claims: &str) -> String {
    let unsigned = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(claims)
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(unsigned.as_bytes());
    let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{unsigned}.{signature}")
}

fn request(method: &str, path: &str, auth: Option<&str>, body: &str) -> CanonicalMessage {
    let msg = CanonicalMessage::from_vec(body.as_bytes())
        .with_metadata_kv("http_method", method)
        .with_metadata_kv("http_path", path);
    match auth {
        Some(auth) => msg.with_metadata_kv("mqb.src.http_authorization", auth),
        None => msg,
    }
}

fn bearer(claims: &str) -> String {
    format!("Bearer {}", token(HS256, claims))
}

/// The status of a rejected request, or 0 if it was accepted.
fn status<T>(result: Result<T, Fail>) -> u16 {
    result.map_or_else(|Fail(status, _)| status, |_| 0)
}

fn text(reply: &Reply) -> &str {
    std::str::from_utf8(&reply.1).unwrap()
}

#[test]
fn positive_id_accepts_only_plain_positive_integers() {
    assert_eq!(positive_id("42"), Some(42));
    for bad in [
        "",
        "0",
        "-1",
        "+1",
        "abc",
        "1.5",
        " 1",
        "99999999999999999999",
    ] {
        assert_eq!(positive_id(bad), None, "{bad:?}");
    }
}

#[test]
fn new_body_trims_and_validates() {
    assert_eq!(
        new_body(br#"{"body":"  hi  "}"#).ok().as_deref(),
        Some("hi")
    );
    let long = format!(r#"{{"body":"{}"}}"#, "é".repeat(501));
    let cases = [
        ("{", "malformed JSON body"),
        ("{}", "body is required"),
        (r#"{"body":5}"#, "body is required"),
        (r#"{"body":"   "}"#, "body is required"),
        (&long, "body must be at most 500 characters"),
    ];
    for (input, expected) in cases {
        let Err(Fail(400, message)) = new_body(input.as_bytes()) else {
            panic!("{input} was accepted");
        };
        assert_eq!(message, expected);
    }
    assert!(new_body(format!(r#"{{"body":"{}"}}"#, "é".repeat(500)).as_bytes()).is_ok());
}

#[tokio::test]
async fn user_accepts_a_valid_token() {
    let api = api().await;
    let msg = request("POST", "/posts", Some(&bearer(ALICE)), "");
    assert!(matches!(api.user(&msg), Ok((7, ref name)) if name == "alice"));
}

#[tokio::test]
async fn user_rejects_bad_tokens() {
    let api = api().await;
    let valid = token(HS256, ALICE);
    let tampered = format!("{}x", &valid[..valid.len() - 1]);
    let cases = [
        (None, "missing bearer token"),
        (Some(format!("Basic {valid}")), "missing bearer token"),
        (
            Some(format!("Bearer {tampered}")),
            "invalid or expired token",
        ),
        (
            Some(format!("Bearer {valid}.extra")),
            "invalid or expired token",
        ),
        (Some("Bearer a.b".to_owned()), "invalid or expired token"),
        (
            Some(format!("Bearer {}", token(r#"{"alg":"none"}"#, ALICE))),
            "invalid or expired token",
        ),
        (
            Some(bearer(r#"{"sub":"7","username":"alice","exp":1}"#)),
            "invalid or expired token",
        ),
        (
            Some(bearer(r#"{"sub":"0","username":"alice","exp":4102444800}"#)),
            "invalid token payload",
        ),
        (
            Some(bearer(r#"{"sub":"7","username":5,"exp":4102444800}"#)),
            "invalid token payload",
        ),
    ];
    for (auth, expected) in cases {
        let msg = request("POST", "/posts", auth.as_deref(), "");
        let Err(Fail(401, message)) = api.user(&msg) else {
            panic!("{auth:?} was accepted");
        };
        assert_eq!(message, expected, "{auth:?}");
    }
}

#[tokio::test]
async fn parse_routes_and_checks_auth_before_id() {
    let api = api().await;
    let auth = bearer(ALICE);
    let cases = [
        ("GET", "/health", None, 0),
        ("GET", "/feed", None, 0),
        ("GET", "/posts/3", None, 0),
        ("GET", "/posts/0", None, 400),
        ("GET", "/posts/3/x", None, 404),
        ("GET", "/nope", None, 404),
        ("POST", "/posts/abc/like", None, 401),
        ("POST", "/posts/abc/like", Some(&auth), 400),
        ("POST", "/posts/3/like", Some(&auth), 0),
        ("POST", "/posts", None, 401),
    ];
    for (method, path, auth, expected) in cases {
        let msg = request(method, path, auth.map(String::as_str), "");
        assert_eq!(status(api.parse(&msg)), expected, "{method} {path}");
    }
}

#[tokio::test]
async fn batch_creates_likes_and_reads_in_one_transaction() {
    let api = api().await;
    let create = Request::Create {
        user: 7,
        name: "alice".to_owned(),
        body: "hello".to_owned(),
    };
    let like = || Request::Like { user: 7, post: 1 };
    let replies = api
        .batch(vec![
            create,
            like(),
            like(),
            Request::Like { user: 7, post: 99 },
            Request::Post(1),
            Request::Post(99),
            Request::Feed,
            Request::Health,
        ])
        .await;
    let statuses: Vec<u16> = replies.iter().map(|r| r.0).collect();
    assert_eq!(statuses, [201, 201, 200, 404, 200, 404, 200, 200]);
    assert!(text(&replies[0]).starts_with(r#"{"post":{"id":1,"body":"hello","created_at":""#));
    assert!(text(&replies[0]).ends_with(r#"","author":"alice","like_count":0}}"#));
    assert_eq!(
        text(&replies[2]),
        r#"{"liked":true,"already_liked":true,"post_id":1}"#
    );
    assert!(text(&replies[4]).ends_with(r#""author":"alice","like_count":1}}"#));
    assert_eq!(text(&replies[5]), r#"{"error":"post not found"}"#);
    assert!(text(&replies[6]).starts_with(r#"{"posts":[{"id":1,"body":"hello""#));
    assert!(text(&replies[7]).starts_with(r#"{"status":"ok","db":"ok","uptime_s":"#));
}

//! Node-zero (scan + catalog area): with SCRYBOX_NODE_UPSTREAM empty, every
//! former forward site in cardscan / cvscan / card_art / model_assets /
//! catalog_build / catalog_read must answer what Node itself would — never
//! the dead-upstream 502 and never a gate-less finalhandler 404.
//!
//! Node references: server.js:634 global handler (body-parser SyntaxError →
//! 500 {"error":"Internal server error"}); routes/cardArt.js
//! (`router.use(authenticateToken)` before POST/DELETE `/:cardId`);
//! routes/admin.js (`router.use(authenticateToken, requireAdmin)`);
//! routes/cardscan.js (`req.body.length < 100` → Missing image).
use axum::{body::Body, extract::Request, http::StatusCode, response::Response};
use scrybox_server::migrations::{run_boot_migration, MigOpts};
use scrybox_server::{app, AppState, Config};

const PW: &str = concat!("Node0-", "scan-", "k4p");

struct H {
    addr: std::net::SocketAddr,
    client: hyper_util::client::legacy::Client<hyper_util::client::legacy::connect::HttpConnector, Body>,
    tok: String,
    _dir: tempfile::TempDir,
}

impl H {
    async fn send(&self, req: Request) -> Response {
        let (mut parts, body) = req.into_parts();
        parts.uri = format!("http://{}{}", self.addr, parts.uri).parse().unwrap();
        let res = self.client.request(Request::from_parts(parts, body)).await.unwrap();
        let (p, b) = res.into_parts();
        Response::from_parts(p, Body::new(b))
    }
    fn req(&self, method: &str, uri: &str, auth: bool, ct: Option<&str>, body: &[u8]) -> Request {
        let mut b = Request::builder().method(method).uri(uri);
        if auth {
            b = b.header("authorization", format!("Bearer {}", self.tok));
        }
        if let Some(ct) = ct {
            b = b.header("content-type", ct);
        }
        b.body(Body::from(body.to_vec())).unwrap()
    }
}

async fn text(res: Response) -> String {
    String::from_utf8_lossy(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).into_owned()
}

async fn setup() -> H {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bindarr.db");
    run_boot_migration(&db, &MigOpts::default()).unwrap();
    let salt = "0123456789abcdef0123456789abcdef";
    let iters = 10_000u32;
    let h = scrybox_server::auth_support::pbkdf2_sha512(PW.as_bytes(), salt.as_bytes(), iters);
    let mut stored = format!("{iters}:{salt}:");
    for b in h {
        stored.push_str(&format!("{b:02x}"));
    }
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO users (username, password_hash, role, share_token, share_enabled)
             VALUES ('admin', ?1, 'admin', 'n0tokn0tokn0tokn0', 0)",
            rusqlite::params![stored],
        )
        .unwrap();
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        node_upstream: String::new(),
        static_dir: None,
        rust_routes: ["*".to_string()].into_iter().collect(),
        db_path: Some(db),
        sidecar_url: None,
    };
    let router = app(AppState::new(cfg));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
    });
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build_http::<Body>();
    let mut hh = H { addr, client, tok: String::new(), _dir: dir };
    let res = hh
        .send(
            Request::post("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"username":"admin","password":"{PW}"}}"#)))
                .unwrap(),
        )
        .await;
    assert!(res.status().is_success(), "login {}", res.status());
    let v: serde_json::Value = serde_json::from_str(&text(res).await).unwrap();
    hh.tok = v["token"].as_str().unwrap().to_string();
    hh
}

const ISE: &str = r#"{"error":"Internal server error"}"#;

async fn assert_ise(h: &H, req: Request) {
    let what = format!("{} {}", req.method(), req.uri());
    let res = h.send(req).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR, "{what}");
    assert_eq!(text(res).await, ISE, "{what}");
}

async fn assert_404(h: &H, req: Request, method: &str, path: &str) {
    let what = format!("{} {}", req.method(), req.uri());
    let res = h.send(req).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND, "{what}");
    assert!(text(res).await.contains(&format!("<pre>Cannot {method} {path}</pre>")), "{what}");
}

#[tokio::test]
async fn cardscan_parse_failures_are_node_global_500() {
    let h = setup().await;
    for path in ["/api/cardscan/cards", "/api/cardscan/choices", "/api/cardscan/scan", "/api/cardscan"] {
        assert_ise(&h, h.req("POST", path, true, Some("application/json"), b"{bad")).await;
        // strict mode: a bare scalar is a SyntaxError too
        assert_ise(&h, h.req("POST", path, true, Some("application/json"), b"5")).await;
    }
    assert_ise(&h, h.req("POST", "/api/cardscan/telemetry", true, Some("text/plain"), b"{bad")).await;
    // detect/frame: the outer bigJson rejects before the route's Missing image.
    assert_ise(&h, h.req("POST", "/api/cardscan/detect", true, Some("application/json"), b"\"x\"")).await;
    assert_ise(&h, h.req("POST", "/api/cardscan/frame", true, Some("application/json"), b"{bad")).await;
}

#[tokio::test]
async fn cardscan_frame_short_raw_image_is_missing_image() {
    let h = setup().await;
    let res = h.send(h.req("POST", "/api/cardscan/frame", true, Some("image/jpeg"), &[0xff; 10])).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(text(res).await, r#"{"ok":false,"error":"Missing image"}"#);
}

#[tokio::test]
async fn scan_match_bad_json_is_global_500() {
    let h = setup().await;
    assert_ise(&h, h.req("POST", "/api/scan-match", true, Some("application/json"), b"{bad")).await;
}

#[tokio::test]
async fn card_art_router_misses_auth_then_404() {
    let h = setup().await;
    // router.use(authenticateToken) runs before the 404.
    let res = h.send(h.req("PUT", "/api/card-art/abc", false, None, b"")).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(text(res).await, r#"{"error":"Access denied. No token provided."}"#);
    assert_404(&h, h.req("PUT", "/api/card-art/abc", true, None, b""), "PUT", "/api/card-art/abc").await;
    // GET without the .png suffix: no GET /:cardId route.
    let res = h.send(h.req("GET", "/api/card-art/abc", false, None, b"")).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_404(&h, h.req("GET", "/api/card-art/abc", true, None, b""), "GET", "/api/card-art/abc").await;
    // normalJson runs first.
    assert_ise(&h, h.req("PUT", "/api/card-art/abc", true, Some("application/json"), b"{bad")).await;
    // `/index` is only special for GET: DELETE walks the /:cardId route.
    let res = h.send(h.req("DELETE", "/api/card-art/index", true, None, b"")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert_eq!(text(res).await, r#"{"error":"No uploaded art for this card"}"#);
    // HEAD rides router.get.
    let res = h.send(h.req("HEAD", "/api/card-art/index", false, None, b"")).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res = h.send(h.req("HEAD", "/api/card-art/nope.png", false, None, b"")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_catalog_and_model_misses_gate_then_404() {
    let h = setup().await;
    // languages: non-GET → auth + requireAdmin → 404
    let res = h.send(h.req("POST", "/api/admin/catalogs/languages", false, None, b"")).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_404(&h, h.req("POST", "/api/admin/catalogs/languages", true, None, b""), "POST", "/api/admin/catalogs/languages").await;
    assert_404(&h, h.req("DELETE", "/api/admin/catalogs/stop", true, None, b""), "DELETE", "/api/admin/catalogs/stop").await;
    assert_404(&h, h.req("GET", "/api/admin/catalogs/build", true, None, b""), "GET", "/api/admin/catalogs/build").await;
    assert_404(&h, h.req("POST", "/api/admin/models", true, None, b""), "POST", "/api/admin/models").await;
    // normalJson SyntaxError → global 500 (pre-auth)
    assert_ise(&h, h.req("POST", "/api/admin/catalogs/build", false, Some("application/json"), b"{bad")).await;
    assert_ise(&h, h.req("POST", "/api/admin/catalogs/stop", false, Some("application/json"), b"{bad")).await;
    assert_ise(&h, h.req("POST", "/api/admin/models/download", false, Some("application/json"), b"{bad")).await;
    // HEAD rides router.get
    let res = h.send(h.req("HEAD", "/api/admin/catalogs/progress", true, None, b"")).await;
    assert_eq!(res.status(), StatusCode::OK);
    // scan-sets: non-GET → normalJson, gate, 404
    assert_ise(&h, h.req("PUT", "/api/scan-sets", true, Some("application/json"), b"{bad")).await;
    assert_404(&h, h.req("PUT", "/api/scan-sets", true, None, b""), "PUT", "/api/scan-sets").await;
}

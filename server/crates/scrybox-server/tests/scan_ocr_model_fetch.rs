//! Lane-B iter-64 pins: the PP-OCRv6 recogniser moved out of the Node-free
//! image and onto the operator-fetch policy (Dockerfile.rust `docker images`
//! SIZE gate). Proves scan_ocr::ensure_model_* — sha256 pin + atomic install,
//! single-flight, disable switch, retryable-vs-fatal classification, redirect
//! following, and the Dockerfile pin-sync guard (the `ocr` builder stage and
//! the runtime constants must not drift). Own binary: env flips.
use scrybox_server::scan_ocr::rec::{self, REC_MODEL_FILE, REC_MODEL_SHA256, REC_MODEL_URL};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

fn payload() -> Vec<u8> {
    (0..65_536u32).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
}

fn sha_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::new().chain_update(bytes).finalize())
}

/// One-shot scripted HTTP server: `status` for every request; 200 carries
/// `payload`. Plain http (the dl_client is https_or_http), connection: close.
fn serve(status: u16, payload: Option<Vec<u8>>, location: Option<String>) -> (SocketAddr, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let resp: Vec<u8> = match (&payload, status) {
                (Some(b), 200) => {
                    let head =
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", b.len());
                    let mut v = head.into_bytes();
                    v.extend_from_slice(b);
                    v
                }
                _ => match &location {
                    Some(loc) => format!(
                        "HTTP/1.1 302 Found\r\nlocation: {loc}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    )
                    .into_bytes(),
                    None => b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
                },
            };
            let _ = s.write_all(&resp);
            let _ = s.flush();
            h.fetch_add(1, Ordering::SeqCst);
        }
    });
    (addr, hits)
}

struct EnvGuard(Vec<(&'static str, Option<String>)>);

impl EnvGuard {
    fn new(keys: &[&'static str]) -> Self {
        // SAFETY: single test in this binary — no other thread reads env here.
        Self(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect())
    }
    fn set(&self, k: &'static str, v: &str) {
        unsafe { std::env::set_var(k, v) };
    }
    fn unset(&self, k: &'static str) {
        unsafe { std::env::remove_var(k) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            // SAFETY: single test in this binary.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

#[test]
fn ocr_recogniser_operator_fetch() {
    let env = EnvGuard::new(&[
        "SCRYBOX_OCR_MODEL_DIR",
        "SCRYBOX_OCR_MODEL_BASE",
        "SCRYBOX_OCR_MODEL_SHA256",
        "SCRYBOX_OCR_MODEL_FETCH",
        "CV_MODEL_DIR",
    ]);
    let body = payload();
    let good_sha = sha_hex(&body);
    let base_dir = std::env::temp_dir().join(format!("ocr-fetch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base_dir);

    // 1. Happy path: missing file -> fetch, sha-verify, atomic install.
    let dir = base_dir.join("happy");
    let (addr, hits) = serve(200, Some(body.clone()), None);
    env.set("SCRYBOX_OCR_MODEL_DIR", &dir.to_string_lossy());
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_SHA256", &good_sha); // mock serves a different body than the pinned file
    env.unset("SCRYBOX_OCR_MODEL_FETCH");
    rec::ensure_model_in(&dir).expect("happy fetch");
    assert_eq!(std::fs::read(dir.join(REC_MODEL_FILE)).unwrap(), body, "installed bytes must match exactly");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "one GET");
    let tmp = dir.join(format!("{REC_MODEL_FILE}.tmp"));
    assert!(!tmp.exists(), "no .tmp residue after install");

    // 2. Present file: Ok without touching the network.
    rec::ensure_model_in(&dir).expect("present short-circuits");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "present file must not re-fetch");

    // 3. sha mismatch: refused, retryable (CDN corruption must not poison
    //    the engine cache), file NOT installed, no tmp left.
    let dir3 = base_dir.join("sha-mismatch");
    let (addr3, hits3) = serve(200, Some(body.clone()), None);
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr3}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_SHA256", &"0".repeat(64));
    let e = rec::ensure_model_in(&dir3).expect_err("mismatched pin must refuse");
    assert!(e.retryable, "sha mismatch on a served body is retryable, got fatal: {e}");
    assert!(e.message.contains("sha256 mismatch"), "{e}");
    assert!(!dir3.join(REC_MODEL_FILE).exists(), "refused body must not install");
    assert!(!dir3.join(format!("{REC_MODEL_FILE}.tmp")).exists(), "refused body must not leave tmp");
    assert_eq!(hits3.load(Ordering::SeqCst), 1);

    // 4. HTTP error: retryable, nothing installed.
    let dir4 = base_dir.join("http404");
    let (addr4, _hits4) = serve(404, None, None);
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr4}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_SHA256", &good_sha);
    let e = rec::ensure_model_in(&dir4).expect_err("404 must fail");
    assert!(e.retryable && e.message.contains("HTTP 404"), "{e}");
    assert!(!dir4.join(REC_MODEL_FILE).exists());

    // 5. Redirect ladder: 302 -> 200 installs (fetch_asset's 10-hop shape).
    let dir5 = base_dir.join("redirect");
    let (addr5b, hits5b) = serve(200, Some(body.clone()), None);
    let (addr5a, hits5a) = serve(302, None, Some(format!("http://{addr5b}/model.onnx")));
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr5a}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_SHA256", &good_sha);
    rec::ensure_model_in(&dir5).expect("302 hop must follow");
    assert_eq!(std::fs::read(dir5.join(REC_MODEL_FILE)).unwrap(), body);
    assert_eq!(hits5a.load(Ordering::SeqCst), 1);
    assert_eq!(hits5b.load(Ordering::SeqCst), 1);

    // 6. Disabled switch: fatal (the pre-iter-64 permanent-offline arm),
    //    no network attempted.
    let dir6 = base_dir.join("disabled");
    let (addr6, hits6) = serve(200, Some(body.clone()), None);
    env.set("SCRYBOX_OCR_MODEL_DIR", &dir6.to_string_lossy());
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr6}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_FETCH", "0");
    let e = rec::ensure_model_in(&dir6).expect_err("disabled fetch must fail");
    assert!(!e.retryable, "disabled + missing is the permanent arm: {e}");
    assert!(e.message.contains("SCRYBOX_OCR_MODEL_FETCH=0"), "{e}");
    assert_eq!(hits6.load(Ordering::SeqCst), 0, "disabled must not hit the network");
    env.unset("SCRYBOX_OCR_MODEL_FETCH");

    // 7. No model dir at all: fatal (nothing to install into).
    env.unset("SCRYBOX_OCR_MODEL_DIR");
    env.unset("CV_MODEL_DIR");
    let e = rec::ensure_model_present().expect_err("no dir must fail");
    assert!(!e.retryable && e.message.contains("no OCR model dir"), "{e}");

    // 8. Single-flight: two racing first-fetches share ONE download.
    let dir8 = base_dir.join("single-flight");
    let (addr8, hits8) = serve(200, Some(body.clone()), None);
    env.set("SCRYBOX_OCR_MODEL_DIR", &dir8.to_string_lossy());
    env.set("SCRYBOX_OCR_MODEL_BASE", &format!("http://{addr8}/{REC_MODEL_FILE}"));
    env.set("SCRYBOX_OCR_MODEL_SHA256", &good_sha);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<(usize, &'static str)>();
    for i in 0..2 {
        let d = dir8.clone();
        let b = barrier.clone();
        let tx = done_tx.clone();
        thread::spawn(move || {
            eprintln!("[case8] racer {i} started");
            b.wait();
            eprintln!("[case8] racer {i} past barrier");
            let r = rec::ensure_model_in(&d);
            eprintln!("[case8] racer {i} ensure done: {:?}", r.as_ref().map(|_| "ok"));
            let _ = tx.send((i, if r.is_ok() { "ok" } else { "err" }));
        });
    }
    drop(done_tx);
    let mut outs = Vec::new();
    for _ in 0..2 {
        match done_rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok((_, o)) => outs.push(o),
            Err(e) => panic!("case8 racer did not finish in 60 s ({e:?}): dir exists={} hits={}", dir8.join(REC_MODEL_FILE).exists(), hits8.load(Ordering::SeqCst)),
        }
    }
    assert!(outs.iter().all(|o| *o == "ok"), "both racers must succeed: {outs:?}");
    assert_eq!(hits8.load(Ordering::SeqCst), 1, "single-flight: one GET for two racers");

    // 9. Pin-sync guard: the constants mirror Dockerfile.rust's `ocr` stage
    //    exactly, and the image must no longer ship the model.
    assert_eq!(REC_MODEL_SHA256.len(), 64);
    assert!(REC_MODEL_SHA256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    let dockerfile = include_str!("../../../../Dockerfile.rust");
    assert!(dockerfile.contains(REC_MODEL_URL), "Dockerfile.rust ocr-stage URL drifted from rec.rs");
    assert!(dockerfile.contains(REC_MODEL_SHA256), "Dockerfile.rust ocr-stage sha drifted from rec.rs");
    assert!(
        !dockerfile.contains("COPY --from=ocr /out/"),
        "iter-64: the recogniser must NOT ship in the image (operator fetch policy)"
    );
    assert!(
        !dockerfile.contains("SCRYBOX_OCR_MODEL_DIR=/opt/ocr"),
        "iter-64: SCRYBOX_OCR_MODEL_DIR=/opt/ocr must be gone so the default falls to $CV_MODEL_DIR/ocr"
    );

    let _ = std::fs::remove_dir_all(&base_dir);
}

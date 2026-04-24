//! End-to-end smoke for the `image` dispatcher using wiremock.
//!
//! Covers:
//! - Gemini HTTP happy-path (mock server returns base64 PNG; PNG written on disk).
//! - MiniMax HTTP happy-path (mock server returns `data.image_base64`).
//! - `UnsupportedKind` error when the chosen backend lacks `kinds=["image"]`.
//! - `MissingApiKey` error when the env var is unset.
//!
//! All tests run inside a single `#[tokio::test]` to avoid env-var races in
//! the default parallel test harness. The test orchestrates scenarios
//! sequentially and resets state between them.

use std::path::PathBuf;

use peer_mcp::image::{dispatch_image, ImageAction, ImageRequest};
use peer_mcp::registry::{BackendKind, BackendSpec, Registry, Transport};

use wiremock::matchers::{header, method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal PNG (8 magic bytes + IHDR fragment) encoded for wiremock responses.
const TINY_PNG_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWNgYGD4DwABBAEA2a6DzAAAAABJRU5ErkJggg==";

fn tiny_png_bytes() -> Vec<u8> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(TINY_PNG_B64).unwrap()
}

fn spec_http(
    name: &str,
    provider: &str,
    base_url: String,
    api_key_env: &str,
) -> BackendSpec {
    BackendSpec {
        name: name.to_string(),
        description: format!("test {name}"),
        command: name.to_string(),
        args: vec![],
        stdin: false,
        env: Default::default(),
        timeout_ms_default: 30_000,
        auth_hint: None,
        kinds: vec![BackendKind::Image],
        transport: Transport::Http,
        provider: Some(provider.to_string()),
        model: Some("test-model".to_string()),
        api_key_env: Some(api_key_env.to_string()),
        base_url: Some(base_url),
        aspect_ratio_default: Some("1:1".to_string()),
        image_template: None,
        image_edit_template: None,
        image_edit_prefix_args: None,
        image_extra_args: None,
    }
}

/// Build a Registry by writing a TOML file and loading via the `$PEER_BACKENDS_TOML`
/// escape hatch. Uses a TempDir so tests don't pollute real config.
fn registry_from_specs(specs: &[BackendSpec], tmp: &tempfile::TempDir) -> Registry {
    let path = tmp.path().join("peer.toml");
    let mut contents = String::new();
    for s in specs {
        contents.push_str("[[backend]]\n");
        contents.push_str(&format!("name = {:?}\n", s.name));
        contents.push_str(&format!("description = {:?}\n", s.description));
        contents.push_str(&format!("command = {:?}\n", s.command));
        contents.push_str(&format!("timeout_ms_default = {}\n", s.timeout_ms_default));
        contents.push_str(&format!(
            "kinds = [{}]\n",
            s.kinds
                .iter()
                .map(|k| format!("{:?}", k.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        contents.push_str(&format!(
            "transport = \"{}\"\n",
            match s.transport {
                Transport::Cli => "cli",
                Transport::Http => "http",
            }
        ));
        if let Some(p) = &s.provider {
            contents.push_str(&format!("provider = {p:?}\n"));
        }
        if let Some(m) = &s.model {
            contents.push_str(&format!("model = {m:?}\n"));
        }
        if let Some(k) = &s.api_key_env {
            contents.push_str(&format!("api_key_env = {k:?}\n"));
        }
        if let Some(b) = &s.base_url {
            contents.push_str(&format!("base_url = {b:?}\n"));
        }
        if let Some(a) = &s.aspect_ratio_default {
            contents.push_str(&format!("aspect_ratio_default = {a:?}\n"));
        }
        contents.push('\n');
    }
    std::fs::write(&path, contents).unwrap();
    // SAFETY: test-only single-threaded env manipulation within a serialized suite.
    std::env::set_var("PEER_BACKENDS_TOML", &path);
    Registry::load().expect("load registry via env override")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_dispatch_smoke() {
    // ------------------------------------------------------------
    // Scenario 1: Gemini HTTP happy-path.
    // ------------------------------------------------------------
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1beta/models/[^/]+:generateContent$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "here's your image"},
                        {"inlineData": {"mimeType": "image/png", "data": TINY_PNG_B64}}
                    ]
                }
            }]
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    // SAFETY: test serialized via single-test function.
    std::env::set_var("PEER_TEST_GEMINI_KEY", "fake-key");
    let spec = spec_http(
        "nanobanana-test",
        "gemini",
        format!("{}/v1beta", server.uri()),
        "PEER_TEST_GEMINI_KEY",
    );
    let registry = registry_from_specs(&[spec], &tmp);

    let out = tmp.path().join("hero.png");
    let req = ImageRequest {
        action: ImageAction::Generate,
        backend: "nanobanana-test".into(),
        prompt: "red cube".into(),
        edit_prompt: None,
        input_path: None,
        output_path: Some(out.clone()),
        aspect_ratio: Some("1:1".into()),
        model: None,
        reference_images: vec![],
        n: 1,
        timeout_ms: Some(20_000),
    };

    let resp = dispatch_image(&registry, req)
        .await
        .expect("gemini happy-path dispatch ok");
    assert_eq!(resp.backend, "nanobanana-test");
    assert_eq!(resp.paths, vec![out.clone()]);
    let bytes = std::fs::read(&out).expect("file written");
    assert_eq!(bytes, tiny_png_bytes(), "bytes match the mock PNG");
    assert!(resp.meta_path.exists(), "meta sidecar persisted");

    // ------------------------------------------------------------
    // Scenario 2: MiniMax HTTP happy-path.
    // ------------------------------------------------------------
    let server2 = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("Authorization", "Bearer fake-minimax"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"image_base64": [TINY_PNG_B64]},
            "base_resp": {"status_code": 0, "status_msg": ""}
        })))
        .mount(&server2)
        .await;

    let tmp2 = tempfile::tempdir().unwrap();
    std::env::set_var("PEER_TEST_MINIMAX_KEY", "fake-minimax");
    let spec2 = spec_http(
        "minimax-image-test",
        "minimax",
        server2.uri(),
        "PEER_TEST_MINIMAX_KEY",
    );
    let registry2 = registry_from_specs(&[spec2], &tmp2);

    let out2 = tmp2.path().join("hero-mini.png");
    let req2 = ImageRequest {
        action: ImageAction::Generate,
        backend: "minimax-image-test".into(),
        prompt: "green sphere".into(),
        edit_prompt: None,
        input_path: None,
        output_path: Some(out2.clone()),
        aspect_ratio: None,
        model: None,
        reference_images: vec![],
        n: 1,
        timeout_ms: Some(20_000),
    };
    let resp2 = dispatch_image(&registry2, req2)
        .await
        .expect("minimax happy-path dispatch ok");
    assert_eq!(resp2.paths, vec![out2.clone()]);
    assert_eq!(std::fs::read(&out2).unwrap(), tiny_png_bytes());

    // ------------------------------------------------------------
    // Scenario 3: MissingApiKey when env var is unset.
    // ------------------------------------------------------------
    std::env::remove_var("PEER_TEST_GEMINI_KEY");
    let tmp3 = tempfile::tempdir().unwrap();
    let spec3 = spec_http(
        "nanobanana-noauth",
        "gemini",
        format!("{}/v1beta", server.uri()),
        "PEER_TEST_GEMINI_KEY",
    );
    let registry3 = registry_from_specs(&[spec3], &tmp3);
    let req3 = ImageRequest {
        action: ImageAction::Generate,
        backend: "nanobanana-noauth".into(),
        prompt: "x".into(),
        edit_prompt: None,
        input_path: None,
        output_path: Some(tmp3.path().join("x.png")),
        aspect_ratio: None,
        model: None,
        reference_images: vec![],
        n: 1,
        timeout_ms: Some(10_000),
    };
    let err3 = dispatch_image(&registry3, req3).await.unwrap_err();
    match err3 {
        peer_mcp::error::PeerError::MissingApiKey { env_var, .. } => {
            assert_eq!(env_var, "PEER_TEST_GEMINI_KEY");
        }
        other => panic!("expected MissingApiKey, got {other:?}"),
    }

    // ------------------------------------------------------------
    // Scenario 4: UnsupportedKind when backend is ask-only.
    // ------------------------------------------------------------
    let tmp4 = tempfile::tempdir().unwrap();
    let mut ask_only = spec_http(
        "ask-only",
        "gemini",
        format!("{}/v1beta", server.uri()),
        "PEER_TEST_GEMINI_KEY",
    );
    ask_only.kinds = vec![BackendKind::Ask];
    let registry4 = registry_from_specs(&[ask_only], &tmp4);
    let req4 = ImageRequest {
        action: ImageAction::Generate,
        backend: "ask-only".into(),
        prompt: "x".into(),
        edit_prompt: None,
        input_path: None,
        output_path: Some(PathBuf::from("/tmp/never-written.png")),
        aspect_ratio: None,
        model: None,
        reference_images: vec![],
        n: 1,
        timeout_ms: Some(10_000),
    };
    let err4 = dispatch_image(&registry4, req4).await.unwrap_err();
    match err4 {
        peer_mcp::error::PeerError::UnsupportedKind { backend, kind } => {
            assert_eq!(backend, "ask-only");
            assert_eq!(kind, "image");
        }
        other => panic!("expected UnsupportedKind, got {other:?}"),
    }

    // Cleanup env vars.
    std::env::remove_var("PEER_BACKENDS_TOML");
    std::env::remove_var("PEER_TEST_MINIMAX_KEY");
}

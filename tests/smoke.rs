//! Smoke tests that prove the native and parsing dependencies actually work on this host.
//!
//! These are infrastructure checks, not feature tests. Network calls go through wiremock
//! only, never against a real endpoint (`AGENTS.md` §5).

use std::error::Error;

#[test]
fn sqlite_bundled_supports_fts5() -> Result<(), Box<dyn Error>> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.execute_batch("CREATE VIRTUAL TABLE m1 USING fts5(path, summary);")?;
    conn.execute(
        "INSERT INTO m1 (path, summary) VALUES (?1, ?2)",
        ("src/lib.rs", "crate root and module wiring"),
    )?;

    let hits: i64 = conn.query_row(
        "SELECT count(*) FROM m1 WHERE m1 MATCH 'wiring'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(hits, 1);
    Ok(())
}

#[test]
fn xml_theme_parses_attributes() -> Result<(), Box<dyn Error>> {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let xml = r#"<theme><color name="accent">#ff0000</color></theme>"#;
    let mut reader = Reader::from_str(xml);
    let mut names = Vec::new();

    loop {
        match reader.read_event()? {
            Event::Start(start) if start.name().as_ref() == "color" => {
                let attribute = start.attributes().next().transpose()?;
                assert!(attribute.is_some(), "color element must carry a name");
                if let Some(attribute) = attribute {
                    names.push(attribute.value.to_string());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    assert_eq!(names, ["accent"]);
    Ok(())
}

#[test]
fn blake3_hashes_are_content_addressed() {
    let first = blake3::hash(b"fn main() {}");
    let second = blake3::hash(b"fn main() {}");
    let other = blake3::hash(b"fn main() { }");

    assert_eq!(first, second);
    assert_ne!(first, other);
}

#[test]
fn tempdir_roundtrip() -> Result<(), Box<dyn Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("AGENTS.md");
    std::fs::write(&path, "digest")?;

    assert_eq!(std::fs::read_to_string(&path)?, "digest");
    Ok(())
}

#[tokio::test]
async fn provider_http_roundtrip_uses_wiremock() -> Result<(), Box<dyn Error>> {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
        .mount(&server)
        .await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", server.uri()))
        .json(&serde_json::json!({ "model": "test" }))
        .send()
        .await?;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await?, r#"{"ok":true}"#);
    Ok(())
}

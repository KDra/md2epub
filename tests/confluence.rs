//! End-to-end tests of `--format confluence` and `md2epub publish`.
//!
//! `publish` talks to an in-process mock of the Confluence Data Center REST
//! API v1 on 127.0.0.1 (a `TcpListener` with a tiny HTTP/1.1 parser) that
//! records every request. No test touches the real network.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

const TOKEN: &str = "s3cr3t-PAT-0123456789abcdef";
const CONTEXT: &str = "/confluence";
const AC_NS: &str = "http://atlassian.com/content";
const RI_NS: &str = "http://atlassian.com/resource/identifier";

// ---------------------------------------------------------------------------
// Mock Confluence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    method: String,
    /// Path without the context path, with the query.
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Req {
    fn is_write(&self) -> bool {
        self.method != "GET"
    }
}

#[derive(Debug, Clone)]
struct MockPage {
    title: String,
    space: String,
    version: u64,
    body: String,
    ancestors: Vec<String>,
}

#[derive(Debug, Clone)]
struct MockAtt {
    id: String,
    page: String,
    title: String,
    data: Vec<u8>,
    comment: String,
}

#[derive(Default)]
struct State {
    pages: HashMap<String, MockPage>,
    atts: Vec<MockAtt>,
    next_id: u64,
    requests: Vec<Req>,
    /// The next PUT answers 409 after another "user" bumped the version.
    conflict_once: bool,
    /// Every request is redirected to `/moved` + its path (a moved base URL).
    redirect: bool,
}

struct Mock {
    state: Arc<Mutex<State>>,
    url: String,
    base: String,
}

impl Mock {
    fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}{CONTEXT}");
        let state = Arc::new(Mutex::new(State {
            next_id: 1000,
            ..State::default()
        }));
        let s = state.clone();
        let b = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let s = s.clone();
                let b = b.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &s, &b);
                });
            }
        });
        Mock {
            state,
            url: base.clone(),
            base,
        }
    }

    fn requests(&self) -> Vec<Req> {
        self.state.lock().unwrap().requests.clone()
    }

    fn writes(&self) -> Vec<(String, String)> {
        self.requests()
            .into_iter()
            .filter(Req::is_write)
            .map(|r| (r.method, r.path.split('?').next().unwrap().to_string()))
            .collect()
    }

    fn add_page(&self, title: &str, version: u64) -> String {
        let mut st = self.state.lock().unwrap();
        st.next_id += 1;
        let id = st.next_id.to_string();
        st.pages.insert(
            id.clone(),
            MockPage {
                title: title.into(),
                space: "DOCS".into(),
                version,
                body: "<p>old</p>".into(),
                ancestors: vec![],
            },
        );
        id
    }

    fn add_att(&self, page: &str, title: &str, data: &[u8], comment: &str) -> String {
        let mut st = self.state.lock().unwrap();
        st.next_id += 1;
        let id = format!("att{}", st.next_id);
        st.atts.push(MockAtt {
            id: id.clone(),
            page: page.into(),
            title: title.into(),
            data: data.to_vec(),
            comment: comment.into(),
        });
        id
    }

    fn page(&self, id: &str) -> MockPage {
        self.state.lock().unwrap().pages[id].clone()
    }

    fn atts(&self, page: &str) -> Vec<MockAtt> {
        self.state
            .lock()
            .unwrap()
            .atts
            .iter()
            .filter(|a| a.page == page)
            .cloned()
            .collect()
    }
}

fn serve(stream: TcpStream, state: &Arc<Mutex<State>>, base: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h)?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    reader.read_exact(&mut body)?;
    let path = target.strip_prefix(CONTEXT).unwrap_or(&target).to_string();
    let req = Req {
        method,
        path,
        headers,
        body,
    };
    let (status, json, redirect) = {
        let mut st = state.lock().unwrap();
        st.requests.push(req.clone());
        let redirect = st.redirect;
        let (status, json) = handle(&mut st, &req, base);
        (status, json, redirect)
    };
    if redirect {
        let origin = base.strip_suffix(CONTEXT).unwrap();
        let mut out = stream;
        write!(
            out,
            "HTTP/1.1 302 Found\r\nLocation: {origin}/moved{target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        return out.flush();
    }
    let text = if json.is_null() {
        String::new()
    } else {
        json.to_string()
    };
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Other",
    };
    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )?;
    out.flush()
}

fn page_json(id: &str, p: &MockPage, base: &str) -> Value {
    json!({
        "id": id,
        "type": "page",
        "status": "current",
        "title": p.title,
        "space": { "key": p.space, "name": "Docs" },
        "version": { "number": p.version },
        "_links": {
            "webui": format!("/display/{}/{}", p.space, p.title.replace(' ', "+")),
            "base": base,
            "context": CONTEXT,
            "self": format!("{base}/rest/api/content/{id}"),
        }
    })
}

fn att_json(a: &MockAtt) -> Value {
    json!({
        "id": a.id,
        "type": "attachment",
        "title": a.title,
        "metadata": { "comment": a.comment, "mediaType": "image/png" },
        "extensions": { "mediaType": "image/png", "fileSize": a.data.len(), "comment": a.comment },
    })
}

fn query(path: &str) -> HashMap<String, String> {
    let q = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| {
            let v = v.replace('+', " ");
            (
                k.to_string(),
                percent_encoding::percent_decode_str(&v)
                    .decode_utf8_lossy()
                    .into_owned(),
            )
        })
        .collect()
}

/// Parses a multipart body into (file name, file bytes, comment).
fn parse_multipart(req: &Req) -> Option<(String, Vec<u8>, String)> {
    let ct = req.headers.get("content-type")?;
    let boundary = format!("--{}", ct.split("boundary=").nth(1)?);
    let body = &req.body;
    let mut parts = Vec::new();
    let mut i = 0;
    let find = |hay: &[u8], needle: &[u8], from: usize| {
        hay[from..]
            .windows(needle.len())
            .position(|w| w == needle)
            .map(|p| p + from)
    };
    while let Some(start) = find(body, boundary.as_bytes(), i) {
        let after = start + boundary.len();
        if body[after..].starts_with(b"--") {
            break;
        }
        let head_end = find(body, b"\r\n\r\n", after)?;
        let next = find(body, boundary.as_bytes(), head_end)?;
        let head = String::from_utf8_lossy(&body[after..head_end]).to_string();
        let content = body[head_end + 4..next - 2].to_vec();
        parts.push((head, content));
        i = next;
    }
    let mut file = None;
    let mut comment = String::new();
    for (head, content) in parts {
        if head.contains("name=\"file\"") {
            let name = head
                .split("filename=\"")
                .nth(1)?
                .split('"')
                .next()?
                .to_string();
            file = Some((name, content));
        } else if head.contains("name=\"comment\"") {
            comment = String::from_utf8_lossy(&content).to_string();
        }
    }
    let (name, data) = file?;
    Some((name, data, comment))
}

fn handle(st: &mut State, req: &Req, base: &str) -> (u16, Value) {
    if req.headers.get("authorization").map(String::as_str) != Some(&format!("Bearer {TOKEN}")) {
        return (
            401,
            json!({"statusCode": 401, "message": "This request requires authentication"}),
        );
    }
    let path = req.path.split('?').next().unwrap().to_string();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", ["rest", "api", "space", key]) => {
            if *key == "DOCS" {
                (200, json!({"key": "DOCS", "name": "Docs"}))
            } else {
                (
                    404,
                    json!({"statusCode": 404, "message": format!("No space with key : {key}")}),
                )
            }
        }
        ("GET", ["rest", "api", "content"]) => {
            let q = query(&req.path);
            let results: Vec<Value> = st
                .pages
                .iter()
                .filter(|(_, p)| {
                    Some(&p.space) == q.get("spaceKey") && Some(&p.title) == q.get("title")
                })
                .map(|(id, p)| page_json(id, p, base))
                .collect();
            let size = results.len();
            (
                200,
                json!({"results": results, "size": size, "start": 0, "limit": 25}),
            )
        }
        ("POST", ["rest", "api", "content"]) => {
            let v: Value = serde_json::from_slice(&req.body).unwrap();
            st.next_id += 1;
            let id = st.next_id.to_string();
            let p = MockPage {
                title: v["title"].as_str().unwrap().into(),
                space: v["space"]["key"].as_str().unwrap().into(),
                version: 1,
                body: v["body"]["storage"]["value"].as_str().unwrap().into(),
                ancestors: v["ancestors"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| x["id"].as_str().unwrap().to_string())
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            assert_eq!(v["type"], "page");
            assert_eq!(v["body"]["storage"]["representation"], "storage");
            let out = page_json(&id, &p, base);
            st.pages.insert(id, p);
            (200, out)
        }
        ("GET", ["rest", "api", "content", id]) => match st.pages.get(*id) {
            Some(p) => (200, page_json(id, p, base)),
            None => (
                404,
                json!({"statusCode": 404, "message": "No content found with id"}),
            ),
        },
        ("PUT", ["rest", "api", "content", id]) => {
            let v: Value = serde_json::from_slice(&req.body).unwrap();
            let conflict = st.conflict_once;
            let Some(p) = st.pages.get_mut(*id) else {
                return (404, json!({"message": "No content found"}));
            };
            if conflict {
                p.version += 1;
                st.conflict_once = false;
                return (
                    409,
                    json!({"statusCode": 409, "message": "Version must be incremented on update."}),
                );
            }
            let n = v["version"]["number"].as_u64().unwrap();
            if n != p.version + 1 {
                return (
                    409,
                    json!({"statusCode": 409, "message": "Version must be incremented on update."}),
                );
            }
            p.version = n;
            p.title = v["title"].as_str().unwrap().into();
            p.body = v["body"]["storage"]["value"].as_str().unwrap().into();
            if let Some(a) = v["ancestors"].as_array() {
                p.ancestors = a.iter().map(|x| x["id"].as_str().unwrap().into()).collect();
            }
            let p = p.clone();
            (200, page_json(id, &p, base))
        }
        ("GET", ["rest", "api", "content", id, "child", "attachment"]) => {
            let q = query(&req.path);
            let start: usize = q.get("start").and_then(|s| s.parse().ok()).unwrap_or(0);
            let limit: usize = q.get("limit").and_then(|s| s.parse().ok()).unwrap_or(25);
            // Pages of at most 2, to exercise pagination.
            let limit = limit.min(2);
            let all: Vec<&MockAtt> = st.atts.iter().filter(|a| a.page == *id).collect();
            let slice: Vec<Value> = all
                .iter()
                .skip(start)
                .take(limit)
                .map(|a| att_json(a))
                .collect();
            let mut links = json!({"base": base, "context": CONTEXT});
            if start + slice.len() < all.len() {
                links["next"] = json!(format!(
                    "/rest/api/content/{id}/child/attachment?limit={limit}&start={}",
                    start + limit
                ));
            }
            let size = slice.len();
            (
                200,
                json!({"results": slice, "start": start, "limit": limit, "size": size, "_links": links}),
            )
        }
        (
            "POST",
            [
                "rest",
                "api",
                "content",
                id,
                "child",
                "attachment",
                rest @ ..,
            ],
        ) => {
            if req.headers.get("x-atlassian-token").map(String::as_str) != Some("no-check") {
                return (403, json!({"message": "XSRF check failed"}));
            }
            let Some((name, data, comment)) = parse_multipart(req) else {
                return (400, json!({"message": "bad multipart"}));
            };
            match rest {
                [] => {
                    if st.atts.iter().any(|a| a.page == *id && a.title == name) {
                        return (
                            400,
                            json!({"message": format!("Cannot add a new attachment with same file name as an existing attachment: {name}")}),
                        );
                    }
                    st.next_id += 1;
                    let a = MockAtt {
                        id: format!("att{}", st.next_id),
                        page: id.to_string(),
                        title: name,
                        data,
                        comment,
                    };
                    let out = json!({"results": [att_json(&a)], "size": 1});
                    st.atts.push(a);
                    (200, out)
                }
                [att_id, "data"] => match st.atts.iter_mut().find(|a| a.id == *att_id) {
                    Some(a) => {
                        assert_eq!(a.title, name);
                        a.data = data;
                        a.comment = comment;
                        let a = a.clone();
                        (200, att_json(&a))
                    }
                    None => (404, json!({"message": "no attachment"})),
                },
                _ => (404, json!({"message": "no such endpoint"})),
            }
        }
        ("DELETE", ["rest", "api", "content", id]) => {
            let before = st.atts.len();
            st.atts.retain(|a| a.id != *id);
            if st.atts.len() < before {
                (204, Value::Null)
            } else {
                (404, json!({"message": "No content found"}))
            }
        }
        _ => (
            404,
            json!({"message": format!("no mock for {} {}", req.method, req.path)}),
        ),
    }
}

// ---------------------------------------------------------------------------
// Running the binary
// ---------------------------------------------------------------------------

struct Run {
    out: Output,
}

impl Run {
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out.stdout).into_owned()
    }
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.out.stderr).into_owned()
    }
    fn ok(self) -> Self {
        assert!(
            self.out.status.success(),
            "failed:\nstdout: {}\nstderr: {}",
            self.stdout(),
            self.stderr()
        );
        self
    }
    fn fails(self) -> Self {
        assert!(
            !self.out.status.success(),
            "should fail:\nstdout: {}\nstderr: {}",
            self.stdout(),
            self.stderr()
        );
        self
    }
}

/// Runs md2epub with an empty user config, the given token, and no
/// proxies. The token must never show up in the output.
fn run(args: &[&str], token: Option<&str>, dir: &Path) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_md2epub"));
    cmd.args(args)
        .current_dir(dir)
        .env("MD2EPUB_CONFIG", dir.join("no-config.toml"))
        .env_remove("MD2EPUB_CONFLUENCE_TOKEN");
    for v in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(v);
    }
    if let Some(t) = token {
        cmd.env("MD2EPUB_CONFLUENCE_TOKEN", t);
    }
    let out = cmd.output().unwrap();
    let run = Run { out };
    for t in [TOKEN, "wrong-token-value-42"] {
        assert!(
            !run.stdout().contains(t),
            "token in stdout: {}",
            run.stdout()
        );
        assert!(
            !run.stderr().contains(t),
            "token in stderr: {}",
            run.stderr()
        );
    }
    run
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// A small document with two images and a code block.
fn doc(dir: &Path) -> PathBuf {
    std::fs::copy(fixtures().join("images/chart.png"), dir.join("chart.png")).unwrap();
    std::fs::copy(
        fixtures().join("images/transparent.png"),
        dir.join("circle.png"),
    )
    .unwrap();
    let md = dir.join("guide.md");
    std::fs::write(
        &md,
        "# Team Guide\n\nSee [setup](#setup).\n\n![Chart](chart.png)\n\n![Circle](circle.png)\n\n## Setup\n\n```bash\necho hi\n```\n",
    )
    .unwrap();
    md
}

fn manifest(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap()
}

fn parse_storage(xml: &str) {
    let wrapped = format!("<root xmlns:ac=\"{AC_NS}\" xmlns:ri=\"{RI_NS}\">{xml}</root>");
    roxmltree::Document::parse(&wrapped).unwrap_or_else(|e| panic!("{e}\n{xml}"));
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[test]
fn export_kitchen_sink_is_well_formed_with_attachments() {
    let tmp = TempDir::new().unwrap();
    let ks = fixtures().join("kitchen-sink.md");
    run(
        &[
            ks.to_str().unwrap(),
            "--format",
            "confluence",
            "--offline",
            "--confluence-toc",
        ],
        None,
        tmp.path(),
    )
    .ok();
    let out = tmp.path().join("kitchen-sink.confluence");
    let page = std::fs::read_to_string(out.join("page.xml")).unwrap();
    parse_storage(&page);
    assert!(page.starts_with("<ac:structured-macro ac:name=\"toc\""));
    assert!(page.contains("<ac:task-list>"));
    assert!(page.contains("ac:name=\"code\""));
    assert!(page.contains("<ri:url ri:value=\"https://www.rust-lang.org/"));
    assert!(!page.contains("<img") && !page.contains("<svg") && !page.contains("<math"));
    let m = manifest(&out);
    assert_eq!(m["title"], "Kitchen Sink");
    let atts = m["attachments"].as_array().unwrap();
    assert!(!atts.is_empty());
    for a in atts {
        let name = a["filename"].as_str().unwrap();
        assert!(
            page.contains(&format!("ri:filename=\"{name}\"")),
            "{name} unused"
        );
        let data = std::fs::read(out.join("attachments").join(name)).unwrap();
        assert_eq!(a["size"].as_u64().unwrap(), data.len() as u64);
        // Attachments are images Confluence shows reliably: no SVG.
        assert_ne!(a["mediaType"], "image/svg+xml");
    }
    // Every referenced attachment is in the manifest.
    for part in page.split("ri:filename=\"").skip(1) {
        let name = part.split('"').next().unwrap();
        assert!(atts.iter().any(|a| a["filename"] == name), "{name} missing");
    }
    #[cfg(feature = "mermaid")]
    {
        // Diagrams are PNGs with transparency.
        let diagram = page
            .split("ac:alt=\"Flowchart\"")
            .nth(1)
            .unwrap()
            .split("ri:filename=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let img = image::open(out.join("attachments").join(diagram)).unwrap();
        assert!(img.color().has_alpha());
    }
}

#[test]
fn export_output_rules() {
    let tmp = TempDir::new().unwrap();
    let md = doc(tmp.path());
    let r = run(
        &[
            md.to_str().unwrap(),
            "--format",
            "confluence",
            "-o",
            "book.epub",
        ],
        None,
        tmp.path(),
    )
    .fails();
    assert!(r.stderr().contains("book.confluence"), "{}", r.stderr());
    run(
        &[
            md.to_str().unwrap(),
            "--format",
            "confluence",
            "-o",
            "out/page",
        ],
        None,
        tmp.path(),
    )
    .ok();
    assert!(tmp.path().join("out/page.confluence/page.xml").is_file());
    // format from book.toml.
    std::fs::write(
        tmp.path().join("book.toml"),
        "inputs = [\"guide.md\"]\nformat = \"confluence\"\noutput = \"b.confluence\"\n",
    )
    .unwrap();
    run(&["build", "book.toml"], None, tmp.path()).ok();
    assert!(tmp.path().join("b.confluence/manifest.json").is_file());
    // A token key in book.toml is refused.
    std::fs::write(
        tmp.path().join("bad.toml"),
        "inputs = [\"guide.md\"]\nconfluence-token = \"x\"\n",
    )
    .unwrap();
    let r = run(&["build", "bad.toml"], None, tmp.path()).fails();
    assert!(r.stderr().contains("environment"), "{}", r.stderr());
}

#[test]
fn config_keys_and_help() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("cfg.toml");
    let with_cfg = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_md2epub"))
            .args(args)
            .env("MD2EPUB_CONFIG", &cfg)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    assert!(with_cfg(&["config", "set", "format", "confluence"]).0);
    assert!(
        with_cfg(&[
            "config",
            "set",
            "confluence-url",
            "https://wiki.example.com/c/"
        ])
        .0
    );
    assert!(with_cfg(&["config", "set", "confluence-space", "DOCS"]).0);
    assert!(with_cfg(&["config", "set", "confluence-parent-id", "123"]).0);
    let (ok, _, err) = with_cfg(&["config", "set", "confluence-token", "abc"]);
    assert!(!ok && err.contains("environment"), "{err}");
    let (_, list, _) = with_cfg(&["config", "list"]);
    assert!(
        list.contains("format") && list.contains("confluence"),
        "{list}"
    );
    assert!(list.contains("https://wiki.example.com/c "), "{list}");
    let (_, help, _) = with_cfg(&["--help"]);
    assert!(help.contains("[default: confluence]"), "{help}");
    let (_, help, _) = with_cfg(&["publish", "--help"]);
    assert!(help.contains("[default: DOCS]"), "{help}");
    assert!(help.contains("[default: 123]"), "{help}");
}

// ---------------------------------------------------------------------------
// Publish
// ---------------------------------------------------------------------------

fn publish_args<'a>(mock: &'a Mock, extra: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["publish"];
    v.extend_from_slice(extra);
    v.extend_from_slice(&["--url", &mock.url, "--space", "DOCS", "--offline"]);
    v
}

#[test]
fn publish_creates_page_then_attachments_then_body() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = doc(tmp.path());
    let parent = mock.add_page("Parent", 1);
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap(), "--parent-id", &parent]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    let writes = mock.writes();
    assert_eq!(writes[0], ("POST".into(), "/rest/api/content".into()));
    let id = mock
        .state
        .lock()
        .unwrap()
        .pages
        .iter()
        .find(|(_, p)| p.title == "Team Guide")
        .map(|(id, _)| id.clone())
        .unwrap();
    let att_path = format!("/rest/api/content/{id}/child/attachment");
    assert_eq!(writes[1], ("POST".into(), att_path.clone()));
    assert_eq!(writes[2], ("POST".into(), att_path));
    assert_eq!(writes[3], ("PUT".into(), format!("/rest/api/content/{id}")));
    assert_eq!(writes.len(), 4);
    let page = mock.page(&id);
    assert_eq!(page.version, 2);
    assert_eq!(page.ancestors, vec![parent]);
    parse_storage(&page.body);
    assert!(page.body.contains("<ac:link ac:anchor=\"setup\">"));
    assert!(
        page.body
            .contains("<ac:parameter ac:name=\"language\">bash</ac:parameter>")
    );
    for a in mock.atts(&id) {
        assert!(page.body.contains(&format!("ri:filename=\"{}\"", a.title)));
        assert!(a.comment.starts_with("md2epub sha256:"));
    }
    // The placeholder only lived in version 1.
    let first: Value = serde_json::from_slice(
        &mock
            .requests()
            .into_iter()
            .find(|r| r.method == "POST" && r.path == "/rest/api/content")
            .unwrap()
            .body,
    )
    .unwrap();
    assert!(
        first["body"]["storage"]["value"]
            .as_str()
            .unwrap()
            .contains("Publishing")
    );
    // Every request authenticates with the bearer token.
    for req in mock.requests() {
        assert_eq!(req.headers["authorization"], format!("Bearer {TOKEN}"));
    }
    assert_eq!(
        r.stdout().trim(),
        format!("{}/display/DOCS/Team+Guide", mock.base)
    );
}

#[test]
fn publish_without_attachments_creates_in_one_request() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("plain.md");
    std::fs::write(&md, "# Plain\n\ntext\n").unwrap();
    run(
        &publish_args(&mock, &[md.to_str().unwrap()]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert_eq!(
        mock.writes(),
        vec![("POST".into(), "/rest/api/content".into())]
    );
}

/// Exports `doc` and returns the export dir and its attachment names.
fn export(tmp: &Path) -> (PathBuf, Vec<String>) {
    let md = doc(tmp);
    run(
        &[md.to_str().unwrap(), "--format", "confluence", "--offline"],
        None,
        tmp,
    )
    .ok();
    let dir = tmp.join("guide.confluence");
    let names = manifest(&dir)["attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["filename"].as_str().unwrap().to_string())
        .collect();
    (dir, names)
}

#[test]
fn publish_exported_dir_updates_skips_changes_and_prunes() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let (dir, names) = export(tmp.path());
    assert_eq!(names.len(), 2);
    let id = mock.add_page("Team Guide", 3);
    let same = std::fs::read(dir.join("attachments").join(&names[0])).unwrap();
    let sha = hex_sha(&same);
    mock.add_att(&id, &names[0], &same, &format!("md2epub sha256:{sha}"));
    let changed_id = mock.add_att(&id, &names[1], b"old bytes", "md2epub sha256:00");
    let stale = mock.add_att(&id, "stale.png", b"x", "");
    let unused = mock.add_att(&id, "keep-me.png", b"y", "");
    let r = run(
        &publish_args(&mock, &[dir.to_str().unwrap(), "--prune-attachments"]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    let writes = mock.writes();
    assert_eq!(
        writes,
        vec![
            (
                "POST".into(),
                format!("/rest/api/content/{id}/child/attachment/{changed_id}/data")
            ),
            ("PUT".into(), format!("/rest/api/content/{id}")),
            // Deleted after the new body is in place, in name order.
            ("DELETE".into(), format!("/rest/api/content/{unused}")),
            ("DELETE".into(), format!("/rest/api/content/{stale}")),
        ]
    );
    assert_eq!(mock.page(&id).version, 4);
    let left: Vec<String> = mock.atts(&id).into_iter().map(|a| a.title).collect();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(
        r.stderr().contains("1 changed, 1 unchanged, 2 deleted"),
        "{}",
        r.stderr()
    );
    // The pages listing was paginated (pages of 2 in the mock).
    let listings = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "GET" && r.path.contains("/child/attachment"))
        .count();
    assert_eq!(listings, 2);
    // No conversion happened: the stored page body is the exported file.
    assert_eq!(
        mock.page(&id).body,
        std::fs::read_to_string(dir.join("page.xml")).unwrap()
    );
}

fn hex_sha(data: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(data))
}

#[test]
fn publish_update_without_prune_keeps_attachments_and_skips_unchanged() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let (dir, names) = export(tmp.path());
    let id = mock.add_page("Team Guide", 1);
    for n in &names {
        let data = std::fs::read(dir.join("attachments").join(n)).unwrap();
        mock.add_att(&id, n, &data, &format!("md2epub sha256:{}", hex_sha(&data)));
    }
    mock.add_att(&id, "manual.png", b"m", "");
    run(
        &publish_args(&mock, &[dir.to_str().unwrap()]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert_eq!(
        mock.writes(),
        vec![("PUT".into(), format!("/rest/api/content/{id}"))]
    );
    assert_eq!(mock.atts(&id).len(), 3);
}

#[test]
fn publish_retries_once_on_version_conflict() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("p.md");
    std::fs::write(&md, "# Conflict Page\n\nx\n").unwrap();
    let id = mock.add_page("Conflict Page", 5);
    mock.state.lock().unwrap().conflict_once = true;
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap(), "--page-id", &id]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    let puts: Vec<Value> = mock
        .requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(puts[0]["version"]["number"], 6);
    assert_eq!(puts[1]["version"]["number"], 7);
    assert_eq!(mock.page(&id).version, 7);
    assert!(r.stderr().contains("conflict"), "{}", r.stderr());
}

#[test]
fn dry_run_makes_no_writes() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = doc(tmp.path());
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap(), "--dry-run"]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(mock.writes().is_empty(), "{:?}", mock.writes());
    assert!(
        r.stdout()
            .contains("would create page \"Team Guide\" in space DOCS"),
        "{}",
        r.stdout()
    );
    assert_eq!(
        r.stdout().matches("would upload").count(),
        2,
        "{}",
        r.stdout()
    );
    // And for an update.
    let id = mock.add_page("Team Guide", 2);
    mock.add_att(&id, "old.png", b"o", "");
    let r = run(
        &publish_args(
            &mock,
            &[md.to_str().unwrap(), "--dry-run", "--prune-attachments"],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(mock.writes().is_empty());
    assert!(r.stdout().contains("version 2 -> 3"), "{}", r.stdout());
    assert!(
        r.stdout().contains("would delete old.png"),
        "{}",
        r.stdout()
    );
}

#[test]
fn auth_errors_are_clear_and_never_show_the_token() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = doc(tmp.path());
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap(), "-v"]),
        Some("wrong-token-value-42"),
        tmp.path(),
    )
    .fails();
    let err = r.stderr();
    assert!(err.contains("401"), "{err}");
    assert!(
        err.contains("This request requires authentication"),
        "{err}"
    );
    assert!(err.contains("token"), "{err}");
    assert!(mock.writes().is_empty());

    // Missing token.
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap()]),
        None,
        tmp.path(),
    )
    .fails();
    assert!(
        r.stderr().contains("MD2EPUB_CONFLUENCE_TOKEN"),
        "{}",
        r.stderr()
    );
    assert!(
        r.stderr().contains("Personal Access Tokens"),
        "{}",
        r.stderr()
    );

    // --token-env names another variable.
    let out = Command::new(env!("CARGO_BIN_EXE_md2epub"))
        .args(publish_args(
            &mock,
            &[md.to_str().unwrap(), "--token-env", "MY_PAT", "-v"],
        ))
        .current_dir(tmp.path())
        .env("MD2EPUB_CONFIG", tmp.path().join("none.toml"))
        .env_remove("MD2EPUB_CONFLUENCE_TOKEN")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env("MY_PAT", TOKEN)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for stream in [&out.stdout, &out.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(TOKEN));
    }

    // The token is refused as an argument, without echoing it.
    for args in [
        vec!["publish", "x.md", "--token", TOKEN],
        vec!["publish", "x.md", &format!("--token={TOKEN}")],
    ] {
        let args: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
        let r = run(&args, None, tmp.path()).fails();
        assert!(
            r.stderr().contains("not accepted as an argument"),
            "{}",
            r.stderr()
        );
    }
}

#[test]
fn missing_space_is_reported() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = doc(tmp.path());
    let r = run(
        &[
            "publish",
            md.to_str().unwrap(),
            "--url",
            &mock.url,
            "--space",
            "NOPE",
            "--offline",
        ],
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    assert!(
        r.stderr().contains("space NOPE not found"),
        "{}",
        r.stderr()
    );
    assert!(mock.writes().is_empty());
}

#[test]
fn publish_settings_come_from_the_user_config() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("cfg.md");
    std::fs::write(&md, "# From Config\n").unwrap();
    std::fs::write(
        tmp.path().join("no-config.toml"),
        format!(
            "confluence-url = \"{}\"\nconfluence-space = \"DOCS\"\n",
            mock.url
        ),
    )
    .unwrap();
    let r = run(&["publish", md.to_str().unwrap()], Some(TOKEN), tmp.path()).ok();
    assert!(
        r.stdout().contains("/display/DOCS/From+Config"),
        "{}",
        r.stdout()
    );
}

// ---------------------------------------------------------------------------
// Regressions
// ---------------------------------------------------------------------------

fn put_bodies(mock: &Mock) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[test]
fn updates_keep_the_parent_unless_parent_id_is_given_on_the_command_line() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n\nx\n").unwrap();
    let saved_parent = mock.add_page("Saved Parent", 1);
    let new_parent = mock.add_page("New Parent", 1);
    let id = mock.add_page("Old", 4);
    {
        let mut st = mock.state.lock().unwrap();
        let p = st.pages.get_mut(&id).unwrap();
        p.space = "OTHER".into();
        p.ancestors = vec!["1".into()];
    }
    std::fs::write(
        tmp.path().join("no-config.toml"),
        format!("confluence-parent-id = \"{saved_parent}\"\n"),
    )
    .unwrap();
    // The saved parent only places new pages: the update has no ancestors.
    let r = run(
        &publish_args(
            &mock,
            &[md.to_str().unwrap(), "--page-id", &id, "--dry-run"],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(!r.stdout().contains("moved"), "{}", r.stdout());
    run(
        &publish_args(&mock, &[md.to_str().unwrap(), "--page-id", &id]),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    let puts = put_bodies(&mock);
    assert_eq!(puts.len(), 1);
    assert!(puts[0].get("ancestors").is_none(), "{}", puts[0]);
    assert_eq!(puts[0]["space"]["key"], "OTHER");
    assert_eq!(mock.page(&id).ancestors, vec!["1".to_string()]);

    // An explicit --parent-id moves the page, and the dry run says so.
    let r = run(
        &publish_args(
            &mock,
            &[
                md.to_str().unwrap(),
                "--page-id",
                &id,
                "--parent-id",
                &new_parent,
                "--dry-run",
            ],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(
        r.stdout()
            .contains(&format!("moved under page {new_parent}")),
        "{}",
        r.stdout()
    );
    assert_eq!(put_bodies(&mock).len(), 1);
    let r = run(
        &publish_args(
            &mock,
            &[
                md.to_str().unwrap(),
                "--page-id",
                &id,
                "--parent-id",
                &new_parent,
            ],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(r.stderr().contains("moved page"), "{}", r.stderr());
    assert_eq!(mock.page(&id).ancestors, vec![new_parent.clone()]);

    // The explicit parent is checked on the --page-id path too.
    let writes = mock.writes().len();
    let r = run(
        &publish_args(
            &mock,
            &[
                md.to_str().unwrap(),
                "--page-id",
                &id,
                "--parent-id",
                "999999",
            ],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    assert!(
        r.stderr().contains("checking --parent-id"),
        "{}",
        r.stderr()
    );
    assert_eq!(mock.writes().len(), writes);
}

#[test]
fn redirects_are_not_followed_and_name_the_new_base() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n").unwrap();
    let id = mock.add_page("T", 1);
    mock.state.lock().unwrap().redirect = true;
    let r = run(
        &publish_args(&mock, &[md.to_str().unwrap(), "--page-id", &id]),
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    let err = r.stderr();
    let origin = mock.base.strip_suffix(CONTEXT).unwrap();
    assert!(err.contains("redirects (HTTP 302)"), "{err}");
    assert!(
        err.contains(&format!("--url {origin}/moved{CONTEXT}")),
        "{err}"
    );
    assert!(
        !err.contains("401") && !err.contains("token is valid"),
        "{err}"
    );
    // Nothing went to the redirect target.
    assert_eq!(mock.requests().len(), 1, "{:?}", mock.requests());
}

#[test]
fn the_token_is_not_sent_over_plain_http_to_another_host() {
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n").unwrap();
    let r = run(
        &[
            "publish",
            md.to_str().unwrap(),
            "--url",
            "http://wiki.example.invalid/c",
            "--space",
            "DOCS",
        ],
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    assert!(r.stderr().contains("plain http://"), "{}", r.stderr());
    assert!(
        r.stderr().contains("--allow-insecure-http"),
        "{}",
        r.stderr()
    );
    // Saving such a URL warns.
    let r = run(
        &[
            "config",
            "set",
            "confluence-url",
            "http://wiki.example.com/c",
        ],
        None,
        tmp.path(),
    )
    .ok();
    assert!(
        r.stderr().contains("--allow-insecure-http"),
        "{}",
        r.stderr()
    );
    let r = run(
        &[
            "config",
            "set",
            "confluence-url",
            "https://wiki.example.com/c",
        ],
        None,
        tmp.path(),
    )
    .ok();
    assert!(!r.stderr().contains("warning"), "{}", r.stderr());
}

#[test]
fn personal_space_keys_and_short_tokens() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n").unwrap();
    run(
        &["config", "set", "confluence-space", "~john@corp.com"],
        None,
        tmp.path(),
    )
    .ok();
    // The key passes the check and reaches the server, percent-encoded.
    let r = run(
        &[
            "publish",
            md.to_str().unwrap(),
            "--url",
            &mock.url,
            "--space",
            "~john@corp.com",
        ],
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    assert!(
        r.stderr().contains("space ~john@corp.com not found"),
        "{}",
        r.stderr()
    );
    assert!(
        mock.requests()
            .iter()
            .any(|q| q.path == "/rest/api/space/%7Ejohn%40corp%2Ecom")
    );
    // A one-letter placeholder token does not garble the message.
    let r = run(
        &[
            "publish",
            md.to_str().unwrap(),
            "--url",
            &mock.url,
            "--space",
            "a b",
        ],
        Some("x"),
        tmp.path(),
    )
    .fails();
    assert!(r.stderr().contains("expected letters"), "{}", r.stderr());
}

#[test]
fn publish_refuses_output_flags_and_warns_about_ignored_conversion_flags() {
    let mock = Mock::start();
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n").unwrap();
    for extra in [
        vec!["-o", "zzz.epub"],
        vec!["--format", "epub"],
        vec!["--output-location", "input"],
    ] {
        let mut args = vec![md.to_str().unwrap(), "--dry-run"];
        args.extend(extra.iter().copied());
        let r = run(&publish_args(&mock, &args), Some(TOKEN), tmp.path()).fails();
        assert!(r.stderr().contains("does not apply"), "{}", r.stderr());
    }
    assert!(!tmp.path().join("zzz.epub").exists());
    assert!(mock.requests().is_empty());

    let (dir, _) = export(tmp.path());
    let r = run(
        &publish_args(
            &mock,
            &[
                dir.to_str().unwrap(),
                "--dry-run",
                "--math",
                "off",
                "--confluence-toc",
            ],
        ),
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    let err = r.stderr();
    assert!(
        err.contains("--math") && err.contains("--confluence-toc") && err.contains("--offline"),
        "{err}"
    );
    assert!(err.contains("ignored"), "{err}");
    // --title applies to an export directory without a warning.
    let r = run(
        &[
            "publish",
            dir.to_str().unwrap(),
            "--url",
            &mock.url,
            "--space",
            "DOCS",
            "--dry-run",
            "--title",
            "Other",
        ],
        Some(TOKEN),
        tmp.path(),
    )
    .ok();
    assert!(!r.stderr().contains("warning"), "{}", r.stderr());
    assert!(r.stdout().contains("\"Other\""), "{}", r.stdout());
    assert!(mock.writes().is_empty());

    // EPUB-only options are not listed in `publish --help`.
    let help = run(&["publish", "--help"], None, tmp.path()).ok().stdout();
    for flag in [
        "--cover",
        "--output-location",
        "--notes-style",
        "--css ",
        "--output ",
    ] {
        assert!(!help.contains(flag), "{flag}: {help}");
    }
    assert!(help.contains("Page title"), "{help}");
}

#[test]
fn a_directory_with_a_foreign_manifest_is_left_alone() {
    let tmp = TempDir::new().unwrap();
    let md = tmp.path().join("t.md");
    std::fs::write(&md, "# T\n").unwrap();
    let webapp = tmp.path().join("webapp");
    std::fs::create_dir_all(webapp.join("attachments")).unwrap();
    let foreign = "{\"name\":\"my-ext\",\"manifest_version\":3}";
    std::fs::write(webapp.join("manifest.json"), foreign).unwrap();
    std::fs::write(webapp.join("attachments/keep.txt"), b"precious").unwrap();
    let r = run(
        &[
            md.to_str().unwrap(),
            "--format",
            "confluence",
            "-o",
            "webapp",
        ],
        None,
        tmp.path(),
    )
    .ok();
    assert!(r.stderr().contains("t.confluence"), "{}", r.stderr());
    assert_eq!(
        std::fs::read(webapp.join("attachments/keep.txt")).unwrap(),
        b"precious"
    );
    assert_eq!(
        std::fs::read_to_string(webapp.join("manifest.json")).unwrap(),
        foreign
    );
    assert!(webapp.join("t.confluence/page.xml").is_file());
    let r = run(
        &[
            "publish",
            "webapp",
            "--url",
            "https://w.example",
            "--space",
            "DOCS",
        ],
        Some(TOKEN),
        tmp.path(),
    )
    .fails();
    assert!(
        r.stderr()
            .contains("not a directory written by --format confluence"),
        "{}",
        r.stderr()
    );
}

#[test]
fn book_toml_with_an_epub_output_builds_despite_a_confluence_default() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("no-config.toml"),
        "format = \"confluence\"\n",
    )
    .unwrap();
    let bk = tmp.path().join("bk");
    std::fs::create_dir(&bk).unwrap();
    std::fs::write(bk.join("ch.md"), "# Ch\n\ntext\n").unwrap();
    std::fs::write(
        bk.join("book.toml"),
        "inputs = [\"ch.md\"]\noutput = \"out/book.epub\"\n",
    )
    .unwrap();
    for args in [vec!["bk/book.toml"], vec!["build", "bk/book.toml"]] {
        let _ = std::fs::remove_dir_all(bk.join("out"));
        run(&args, None, tmp.path()).ok();
        assert!(bk.join("out/book.epub").is_file(), "{args:?}");
    }
    // Without an .epub output the user default still applies.
    run(&["bk/ch.md"], None, tmp.path()).ok();
    assert!(tmp.path().join("ch.confluence/manifest.json").is_file());
}

#[test]
fn linked_documents_keep_linked_images_and_number_footnotes_through() {
    let tmp = TempDir::new().unwrap();
    std::fs::copy(fixtures().join("photo.jpg"), tmp.path().join("photo.jpg")).unwrap();
    std::fs::write(
        tmp.path().join("a.md"),
        "# A Doc\n\n[![photo](photo.jpg)](b.md) [![p2](photo.jpg)](#a-doc)\n\nNote.[^n]\n\n[^n]: First note.\n",
    )
    .unwrap();
    std::fs::write(
        tmp.path().join("b.md"),
        "# B Doc\n\nOther.[^n]\n\n[^n]: Second note.\n",
    )
    .unwrap();
    let r = run(
        &["a.md", "--format", "confluence", "--math", "off"],
        None,
        tmp.path(),
    )
    .ok();
    assert!(
        r.stderr().contains("1 attachments, 0 warnings"),
        "{}",
        r.stderr()
    );
    let out = tmp.path().join("a.confluence");
    let page = std::fs::read_to_string(out.join("page.xml")).unwrap();
    parse_storage(&page);
    let m = manifest(&out);
    let photo = m["attachments"][0]["filename"]
        .as_str()
        .unwrap()
        .to_string();
    for alt in ["photo", "p2"] {
        assert!(
            page.contains(&format!(
                "<ac:link-body><ac:image ac:alt=\"{alt}\"><ri:attachment ri:filename=\"{photo}\" /></ac:image></ac:link-body>"
            )),
            "{alt}: {page}"
        );
    }
    assert!(
        page.contains("<![CDATA[1]]>") && page.contains("<![CDATA[2]]>"),
        "{page}"
    );
    let notes = &page[page.find("<h2>Footnotes</h2>").unwrap()..];
    assert_eq!(notes.matches("<ol>").count(), 1, "{page}");
}

use std::{
    fmt::Write as _,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, RwLock,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};

use crate::{
    app::{AppState, Msg},
    model::{
        CheckState, ChecksRollup, Pr, PrTreeNode, ReviewState, authored_tree,
        checks_by_display_priority, group_by_repo,
    },
    report::{
        self, ChecksSummary, Row, build_section_merged, checks_summary_text, reviewer_summary,
    },
};

pub const DEFAULT_ADDRESS: &str = "127.0.0.1:7011";

#[derive(Clone, Debug)]
pub struct WebSnapshot {
    viewer: String,
    authored: Vec<Pr>,
    reviewing: Vec<Pr>,
    merged: Vec<Pr>,
    loaded_at: Option<DateTime<Local>>,
    error: Option<String>,
    status: Option<String>,
    loading: bool,
}

impl Default for WebSnapshot {
    fn default() -> Self {
        Self {
            viewer: String::new(),
            authored: Vec::new(),
            reviewing: Vec::new(),
            merged: Vec::new(),
            loaded_at: None,
            error: None,
            status: None,
            loading: true,
        }
    }
}

impl WebSnapshot {
    pub fn from_app(state: &AppState) -> Self {
        Self {
            viewer: state.viewer_str().to_string(),
            authored: state.authored.clone(),
            reviewing: state.reviewing.clone(),
            merged: state.merged.clone(),
            loaded_at: state.loaded_at,
            error: state.error.clone(),
            status: state.status.clone(),
            loading: state.loading,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_loading(&self) -> bool {
        self.loading
    }
}

#[derive(Clone)]
pub struct SnapshotStore(Arc<RwLock<Arc<WebSnapshot>>>);

impl SnapshotStore {
    pub(crate) fn new() -> Self {
        Self(Arc::new(RwLock::new(Arc::new(WebSnapshot::default()))))
    }

    pub fn publish(&self, snapshot: WebSnapshot) {
        let mut current = self.0.write().unwrap_or_else(|e| e.into_inner());
        *current = Arc::new(snapshot);
    }

    pub(crate) fn load(&self) -> Arc<WebSnapshot> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

pub struct WebServer {
    snapshots: SnapshotStore,
    #[cfg(test)]
    address: std::net::SocketAddr,
    shutdown: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl WebServer {
    pub fn snapshots(&self) -> SnapshotStore {
        self.snapshots.clone()
    }

    #[cfg(test)]
    fn local_addr(&self) -> std::net::SocketAddr {
        self.address
    }
}

impl Drop for WebServer {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(address: &str, refresh_requests: Sender<Msg>) -> Result<WebServer> {
    let listener = TcpListener::bind(address)
        .with_context(|| format!("could not bind web dashboard at {address}"))?;
    listener
        .set_nonblocking(true)
        .context("configuring web dashboard listener")?;
    #[cfg(test)]
    let bound_address = listener
        .local_addr()
        .context("reading web dashboard listener address")?;
    let snapshots = SnapshotStore::new();
    let worker_snapshots = snapshots.clone();
    let (shutdown, shutdown_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let snapshot = worker_snapshots.load();
                    let _ = serve_connection(stream, &snapshot, &refresh_requests);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if shutdown_rx.recv_timeout(Duration::from_millis(25)).is_ok() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    Ok(WebServer {
        snapshots,
        #[cfg(test)]
        address: bound_address,
        shutdown,
        worker: Some(worker),
    })
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, String)>,
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    host: Option<String>,
    origin: Option<String>,
    fetch_site: Option<String>,
}

fn serve_connection(
    mut stream: TcpStream,
    snapshot: &WebSnapshot,
    refresh_requests: &Sender<Msg>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = [0_u8; 8192];
    let mut read = 0;
    while read < bytes.len() {
        let count = stream.read(&mut bytes[read..])?;
        if count == 0 {
            break;
        }
        read += count;
        if bytes[..read].windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let response = match parse_request(&bytes[..read]) {
        Some(request) => route(snapshot, &request, refresh_requests),
        None => text_response("400 Bad Request", "bad request\n"),
    };
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n",
        response.status,
        response.content_type,
        response.body.len(),
    )?;
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n{}", response.body)?;
    stream.flush()
}

fn parse_request(bytes: &[u8]) -> Option<Request> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.contains("\r\n\r\n") {
        return None;
    }
    let mut lines = text.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    if request_line.next()? != "HTTP/1.1" || request_line.next().is_some() {
        return None;
    }
    let mut request = Request {
        method,
        target,
        host: None,
        origin: None,
        fetch_site: None,
    };
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "host" => request.host = Some(value.trim().to_string()),
            "origin" => request.origin = Some(value.trim().to_string()),
            "sec-fetch-site" => request.fetch_site = Some(value.trim().to_ascii_lowercase()),
            _ => {}
        }
    }
    Some(request)
}

fn route(snapshot: &WebSnapshot, request: &Request, refresh_requests: &Sender<Msg>) -> Response {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/") => Response {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: render_authored(snapshot),
            headers: Vec::new(),
        },
        ("GET", "/merged") => Response {
            status: "200 OK",
            content_type: "text/html; charset=utf-8",
            body: render_merged(snapshot, Utc::now()),
            headers: Vec::new(),
        },
        ("POST", "/refresh?return=%2F") | ("POST", "/refresh?return=%2Fmerged") => {
            if !same_origin(request) {
                return text_response("403 Forbidden", "cross-origin refresh rejected\n");
            }
            let return_to = if request.target.ends_with("%2Fmerged") {
                "/merged"
            } else {
                "/"
            };
            let (acknowledged, acknowledgment) = mpsc::channel();
            if refresh_requests
                .send(Msg::WebRefresh { acknowledged })
                .is_err()
                || acknowledgment.recv_timeout(Duration::from_secs(2)).is_err()
            {
                return text_response("503 Service Unavailable", "refresh unavailable\n");
            }
            Response {
                status: "303 See Other",
                content_type: "text/plain; charset=utf-8",
                body: String::new(),
                headers: vec![("Location", return_to.to_string())],
            }
        }
        ("GET", target) if target.starts_with("/refresh") => method_not_allowed("POST"),
        ("POST", "/") | ("POST", "/merged") => method_not_allowed("GET"),
        (_, target) if target == "/" || target == "/merged" || target.starts_with("/refresh") => {
            method_not_allowed(if target.starts_with("/refresh") {
                "POST"
            } else {
                "GET"
            })
        }
        _ => text_response("404 Not Found", "404 not found\n"),
    }
}

fn same_origin(request: &Request) -> bool {
    if request.fetch_site.as_deref() == Some("cross-site") {
        return false;
    }
    match (&request.origin, &request.host) {
        (None, _) => true,
        (Some(origin), Some(host)) => origin.trim_end_matches('/') == format!("http://{host}"),
        (Some(_), None) => false,
    }
}

fn method_not_allowed(allow: &'static str) -> Response {
    let mut response = text_response("405 Method Not Allowed", "method not allowed\n");
    response.headers.push(("Allow", allow.to_string()));
    response
}

fn text_response(status: &'static str, body: &str) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8",
        body: body.to_string(),
        headers: Vec::new(),
    }
}

fn page_start(title: &str, current: &str, snapshot: &WebSnapshot) -> String {
    let mut out = String::new();
    let _ = write!(
        out,
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{} · rollup</title><style>{}</style></head><body data-loading=\"{}\"><header><div><strong>rollup</strong><span class=\"viewer\">{}</span></div><nav aria-label=\"Dashboard\"><a href=\"/\"{}>Authored by me</a><a href=\"/merged\"{}>Recently merged</a></nav></header><main>",
        escape(title),
        CSS,
        snapshot.loading,
        if snapshot.viewer.is_empty() {
            String::new()
        } else {
            format!("@{}", escape(&snapshot.viewer))
        },
        if current == "/" {
            " aria-current=\"page\""
        } else {
            ""
        },
        if current == "/merged" {
            " aria-current=\"page\""
        } else {
            ""
        },
    );
    render_status(&mut out, snapshot);
    out
}

fn render_status(out: &mut String, snapshot: &WebSnapshot) {
    let mut has_status = false;
    let mut status = String::from("<section class=\"status\" aria-label=\"Data status\">");
    if snapshot.loading {
        status.push_str(
            "<p><span class=\"spinner\" aria-hidden=\"true\"></span>Loading GitHub data…</p>",
        );
        has_status = true;
    }
    if let Some(error) = &snapshot.error {
        let _ = write!(
            status,
            "<p class=\"error\" role=\"alert\">Refresh failed: {}</p>",
            escape(error)
        );
        has_status = true;
    }
    if let Some(note) = &snapshot.status {
        let _ = write!(status, "<p class=\"warning\">{}</p>", escape(note));
        has_status = true;
    }
    if let Some(loaded_at) = snapshot.loaded_at {
        let _ = write!(
            status,
            "<p class=\"loaded\">Latest successful refresh: <time datetime=\"{}\">{}</time></p>",
            loaded_at.to_rfc3339(),
            loaded_at.format("%Y-%m-%d %H:%M:%S %Z")
        );
        has_status = true;
    }
    status.push_str("</section>");
    if has_status {
        out.push_str(&status);
    }
}

fn render_authored(snapshot: &WebSnapshot) -> String {
    let mut out = page_start("Authored by me", "/", snapshot);
    let _ = write!(
        out,
        "<div class=\"page-title\"><div><h1>Authored by me</h1><p>{} open pull request{}</p></div>",
        snapshot.authored.len(),
        if snapshot.authored.len() == 1 {
            ""
        } else {
            "s"
        }
    );
    render_refresh(&mut out, "/", snapshot.loading);
    out.push_str("</div>");
    if snapshot.authored.is_empty() {
        out.push_str("<p class=\"empty\">No open authored pull requests.</p>");
    } else {
        for (repo, prs) in group_by_repo(&snapshot.authored) {
            let _ = write!(
                out,
                "<section class=\"repo\"><h2>{}</h2><ul class=\"pr-tree\">",
                escape(&repo)
            );
            for node in authored_tree(&prs) {
                render_pr(&mut out, &node);
            }
            out.push_str("</ul></section>");
        }
    }
    page_end(&mut out);
    out
}

fn render_pr(out: &mut String, node: &PrTreeNode<'_>) {
    let pr = node.pr;
    out.push_str("<li class=\"pr\"><article><h3>");
    let _ = write!(
        out,
        "<a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\"><span class=\"number\">#{}</span> {}</a>{}",
        escape(&pr.url),
        pr.number,
        escape(&pr.title),
        if pr.is_draft {
            " <span class=\"badge\">draft</span>"
        } else {
            ""
        }
    );
    out.push_str("</h3>");

    if !pr.checks.is_empty() {
        let summary = ChecksSummary::of(pr);
        let class = match summary.rollup {
            ChecksRollup::Green => "ok",
            ChecksRollup::Red => "bad",
            ChecksRollup::Pending => "pending",
            ChecksRollup::Unknown => "muted",
        };
        let _ = write!(
            out,
            "<details class=\"pr-section checks\" data-state-key=\"{}\"><summary>Checks <span class=\"{}\">{}</span></summary><p class=\"context-link\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">Open PR</a></p><ul>",
            escape(&section_state_key(pr, "checks")),
            class,
            escape(&format!(
                "{} — {}",
                checks_rollup_label(summary.rollup),
                checks_summary_text(&summary)
            )),
            escape(&pr.url)
        );
        for check in checks_by_display_priority(&pr.checks) {
            let target = check
                .url
                .as_deref()
                .filter(|url| !url.is_empty())
                .unwrap_or(&pr.url);
            let _ = write!(
                out,
                "<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\"><span class=\"state {}\" aria-hidden=\"true\">{}</span><span class=\"check-state-label\">{}</span> {}{}</a></li>",
                escape(target),
                check_state_class(check.state),
                check_state_symbol(check.state),
                check_state_label(check.state),
                escape(&check.name),
                if check.required {
                    " <span class=\"required-marker\" title=\"Required check\" aria-label=\"required check\">◆</span>"
                } else {
                    ""
                }
            );
        }
        out.push_str("</ul></details>");
    }

    if !pr.reviewers.is_empty() {
        let summary = reviewer_summary(&pr.reviewers)
            .iter()
            .map(|token| token.console_label())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            out,
            "<details class=\"pr-section reviewers\" data-state-key=\"{}\"><summary>Reviewers <span class=\"muted\">[{}]</span></summary><p class=\"context-link\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">Open PR</a></p><ul>",
            escape(&section_state_key(pr, "reviewers")),
            escape(&summary),
            escape(&pr.url)
        );
        for reviewer in &pr.reviewers {
            let reviewed = if reviewer.requested {
                "[req]"
            } else {
                "(reviewed)"
            };
            let _ = write!(
                out,
                "<li><span class=\"state\">{}</span> {} <span class=\"muted\">{} {}</span> <a class=\"row-link\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">PR</a></li>",
                review_state_symbol(reviewer.state),
                escape(&reviewer.login),
                review_state_label(reviewer.state),
                reviewed,
                escape(&pr.url)
            );
        }
        out.push_str("</ul></details>");
    }

    if !pr.unresolved_comments.is_empty() {
        let _ = write!(
            out,
            "<details class=\"pr-section comments\" data-state-key=\"{}\" open><summary>Open comments</summary><p class=\"context-link\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">Open PR</a></p><ul>",
            escape(&section_state_key(pr, "comments")),
            escape(&pr.url)
        );
        for comment in &pr.unresolved_comments {
            let path = comment
                .path
                .as_deref()
                .map(|p| format!(" ({})", escape(p)))
                .unwrap_or_default();
            let outdated = if comment.is_outdated {
                " <span class=\"badge\">outdated</span>"
            } else {
                ""
            };
            let _ = write!(
                out,
                "<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">@{} — {}{}{}</a></li>",
                escape(&comment.url),
                escape(&comment.author),
                escape(&comment.body),
                path,
                outdated
            );
        }
        out.push_str("</ul></details>");
    }

    if !node.children.is_empty() {
        let _ = write!(
            out,
            "<details class=\"pr-section stacked\" data-state-key=\"{}\" open><summary>Stacked PRs</summary><p class=\"context-link\"><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">Open PR</a></p><ul class=\"pr-tree\">",
            escape(&section_state_key(pr, "stacked")),
            escape(&pr.url)
        );
        for child in &node.children {
            render_pr(out, child);
        }
        out.push_str("</ul></details>");
    }
    out.push_str("</article></li>");
}

fn render_merged(snapshot: &WebSnapshot, now: DateTime<Utc>) -> String {
    let mut out = page_start("Recently merged PRs", "/merged", snapshot);
    let allowed = report::allowed_authors_me(&snapshot.viewer, &snapshot.reviewing);
    let section = build_section_merged(&snapshot.merged, &allowed, report::MERGED_PANE_CAP, now);
    let _ = write!(
        out,
        "<div class=\"page-title\"><div><h1>Recently merged PRs</h1><p>{} pull request{}</p></div>",
        section.count,
        if section.count == 1 { "" } else { "s" }
    );
    render_refresh(&mut out, "/merged", snapshot.loading);
    out.push_str("</div>");
    if section.rows.is_empty() {
        out.push_str("<p class=\"empty\">No recently merged PRs.</p>");
    } else {
        out.push_str("<ol class=\"merged-list\">");
        for row in section.rows {
            if let Row::MergedPr { pr, .. } = row {
                let age = pr
                    .merged_at
                    .map(|at| crate::model::human_age(at, now))
                    .unwrap_or_default();
                let _ = write!(
                    out,
                    "<li><a href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\"><span class=\"number\">#{}</span> {}</a><div class=\"meta\">{} · {} · @{}</div></li>",
                    escape(&pr.url),
                    pr.number,
                    escape(&pr.title),
                    escape(&age),
                    escape(&pr.repo),
                    escape(&pr.author)
                );
            }
        }
        out.push_str("</ol>");
    }
    page_end(&mut out);
    out
}

fn render_refresh(out: &mut String, return_to: &str, loading: bool) {
    let encoded_return = if return_to == "/merged" {
        "%2Fmerged"
    } else {
        "%2F"
    };
    let _ = write!(
        out,
        "<form class=\"refresh\" method=\"post\" action=\"/refresh?return={}\"><button type=\"submit\"{}{}>{}</button></form>",
        encoded_return,
        if loading { " disabled" } else { "" },
        if loading {
            " aria-disabled=\"true\" aria-busy=\"true\""
        } else {
            ""
        },
        if loading { "Refreshing…" } else { "Refresh" }
    );
}

fn section_state_key(pr: &Pr, section: &str) -> String {
    format!("repo:{}:pr:{}:section:{section}", pr.repo, pr.number)
}

fn page_end(out: &mut String) {
    out.push_str("</main><script>");
    out.push_str(SCRIPT);
    out.push_str("</script></body></html>");
}

fn check_state_symbol(state: CheckState) -> &'static str {
    match state {
        CheckState::Success => "✓",
        CheckState::Failure | CheckState::Error => "✗",
        CheckState::Pending => "◉",
        CheckState::Skipped => "⊘",
        CheckState::Neutral => "○",
    }
}

fn check_state_class(state: CheckState) -> &'static str {
    match state {
        CheckState::Success => "ok",
        CheckState::Failure | CheckState::Error => "bad",
        CheckState::Pending => "pending",
        CheckState::Skipped | CheckState::Neutral => "muted",
    }
}

fn check_state_label(state: CheckState) -> &'static str {
    match state {
        CheckState::Success => "success",
        CheckState::Failure => "failure",
        CheckState::Pending => "pending",
        CheckState::Neutral => "neutral",
        CheckState::Skipped => "skipped",
        CheckState::Error => "error",
    }
}

fn checks_rollup_label(rollup: ChecksRollup) -> &'static str {
    match rollup {
        ChecksRollup::Green => "green",
        ChecksRollup::Red => "failing",
        ChecksRollup::Pending => "pending",
        ChecksRollup::Unknown => "unknown",
    }
}

fn review_state_symbol(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "✓",
        ReviewState::ChangesRequested => "✗",
        ReviewState::Commented => "◉",
        ReviewState::Dismissed => "⊘",
        ReviewState::NoReview => "○",
    }
}

fn review_state_label(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "changes requested",
        ReviewState::Commented => "commented",
        ReviewState::Dismissed => "dismissed",
        ReviewState::NoReview => "no review",
    }
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const SCRIPT: &str = r#"
(() => {
  const prefix = "rollup:details:";
  for (const details of document.querySelectorAll("details[data-state-key]")) {
    const storageKey = prefix + details.dataset.stateKey;
    try {
      const stored = sessionStorage.getItem(storageKey);
      if (stored !== null) details.open = stored === "open";
    } catch (_) {}
    details.addEventListener("toggle", () => {
      try {
        sessionStorage.setItem(storageKey, details.open ? "open" : "closed");
      } catch (_) {}
    });
  }
  if (document.body.dataset.loading === "true") {
    window.setTimeout(() => window.location.reload(), 750);
  }
})();
"#;

const CSS: &str = r#"
:root{color-scheme:light dark;--bg:#f7f7f4;--panel:#fff;--text:#20221f;--muted:#686d66;--line:#d8dbd5;--link:#145ea8;--ok:#17813d;--bad:#ba2b2b;--pending:#987000}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--text);font:15px/1.45 ui-sans-serif,system-ui,sans-serif}body>header{position:sticky;top:0;z-index:2;display:flex;justify-content:space-between;align-items:center;gap:1rem;padding:.8rem max(1rem,calc((100% - 960px)/2));background:var(--panel);border-bottom:1px solid var(--line)}nav{display:flex;gap:.4rem}nav a{padding:.35rem .6rem;border-radius:.4rem;text-decoration:none}nav a[aria-current=page]{background:var(--link);color:#fff}.viewer,.muted,.meta,.loaded{color:var(--muted)}.viewer{margin-left:.6rem}main{max-width:960px;margin:0 auto;padding:1.4rem 1rem 4rem}.status{padding:.7rem 1rem;margin-bottom:1rem;border:1px solid var(--line);border-radius:.5rem;background:var(--panel)}.status p{margin:.2rem 0}.error,.bad{color:var(--bad)}.warning,.pending{color:var(--pending)}.ok{color:var(--ok)}.page-title{display:flex;justify-content:space-between;align-items:end;gap:1rem}.page-title h1{margin:0}.page-title p{margin:.15rem 0 0;color:var(--muted)}.refresh button{padding:.45rem .8rem;border:1px solid var(--line);border-radius:.4rem;background:var(--panel);color:var(--text);font:inherit;cursor:pointer}.refresh button:hover:not(:disabled){border-color:var(--link);color:var(--link)}.refresh button:disabled{cursor:wait;color:var(--muted)}.repo{margin-top:1.6rem}.repo h2{font-size:1rem;color:var(--muted);border-bottom:1px solid var(--line);padding-bottom:.35rem}.pr-tree,.pr-section ul{list-style:none;padding-left:1rem}.pr{position:relative;margin:.55rem 0;padding-left:.8rem;border-left:2px solid var(--line)}.pr article>h3{font-size:1rem;margin:.2rem 0}.pr a,.merged-list a{color:var(--link)}.number{font-variant-numeric:tabular-nums}.badge{display:inline-block;padding:.05rem .35rem;border:1px solid var(--line);border-radius:999px;color:var(--muted);font-size:.78rem}.pr-section{margin:.35rem 0 .35rem .2rem}.pr-section>summary{cursor:pointer;font-weight:600}.pr-section ul{margin:.3rem 0}.pr-section li{margin:.25rem 0}.context-link{margin:.25rem 0 .25rem 1rem;font-size:.85rem}.row-link{margin-left:.35rem}.state{display:inline-block;min-width:1.2rem}.required-marker{color:var(--muted);font-size:.7em;vertical-align:.15em}.merged-list{padding:0;list-style:none}.merged-list li{padding:.8rem 1rem;margin:.65rem 0;background:var(--panel);border:1px solid var(--line);border-radius:.5rem}.meta{font-size:.85rem;margin-top:.2rem}.empty{padding:2rem;text-align:center;color:var(--muted)}@media(max-width:600px){body>header{position:static;display:block}nav{margin-top:.6rem;overflow-x:auto}.pr-tree,.pr-section ul{padding-left:.45rem}.pr{padding-left:.55rem}main{padding-top:1rem}}@media(prefers-color-scheme:dark){:root{--bg:#151715;--panel:#1e211e;--text:#e5e8e3;--muted:#a4aaa1;--line:#3b403a;--link:#75b7ff;--ok:#66ce84;--bad:#ff8585;--pending:#e9c45d}}
"#;

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::model::{CheckStatus, ChecksRollup, PrComment, ReviewerKind, ReviewerStatus};

    fn pr(repo: &str, number: u64, author: &str, updated: i64) -> Pr {
        Pr {
            number,
            title: format!("title {number}"),
            url: format!("https://example.test/{repo}/{number}"),
            is_draft: false,
            repo: repo.to_string(),
            base_ref: "main".to_string(),
            head_ref: format!("branch-{number}"),
            author: author.to_string(),
            reviewers: Vec::new(),
            updated_at: Utc.timestamp_opt(updated, 0).unwrap(),
            merged_at: None,
            unresolved_comments: Vec::new(),
            checks: Vec::new(),
            checks_rollup: ChecksRollup::Unknown,
        }
    }

    fn rich_snapshot() -> WebSnapshot {
        let mut parent = pr("z/repo", 1, "me", 20);
        parent.is_draft = true;
        parent.title = "Parent <fix>".to_string();
        parent.url = "https://example.test/?a=1&b=\"two\"".to_string();
        parent.reviewers = vec![
            ReviewerStatus {
                login: "alice<script>".to_string(),
                kind: ReviewerKind::User,
                state: ReviewState::ChangesRequested,
                requested: false,
            },
            ReviewerStatus {
                login: "bob".to_string(),
                kind: ReviewerKind::User,
                state: ReviewState::Approved,
                requested: true,
            },
        ];
        parent.checks = vec![
            CheckStatus {
                name: "required & good".to_string(),
                state: CheckState::Success,
                url: Some("https://checks.test/?x=1&y=2".to_string()),
                required: true,
            },
            CheckStatus {
                name: "optional".to_string(),
                state: CheckState::Failure,
                url: None,
                required: false,
            },
        ];
        parent.checks_rollup = ChecksRollup::Green;
        parent.unresolved_comments = vec![PrComment {
            author: "eve<img>".to_string(),
            body: "say <hello> & goodbye".to_string(),
            url: "https://comments.test/?q=\"x\"&a=1".to_string(),
            path: Some("src/<bad>.rs".to_string()),
            is_outdated: true,
        }];
        let mut child = pr("z/repo", 2, "me", 10);
        child.base_ref = parent.head_ref.clone();
        WebSnapshot {
            viewer: "me".to_string(),
            authored: vec![parent, child, pr("A/repo", 3, "me", 30)],
            reviewing: vec![pr("r/review", 9, "bob", 0)],
            merged: Vec::new(),
            loaded_at: None,
            error: None,
            status: None,
            loading: false,
        }
    }

    fn request(method: &str, target: &str) -> Request {
        Request {
            method: method.to_string(),
            target: target.to_string(),
            host: Some("127.0.0.1:7011".to_string()),
            origin: None,
            fetch_site: None,
        }
    }

    fn state_keys(html: &str) -> Vec<&str> {
        html.split("data-state-key=\"")
            .skip(1)
            .map(|rest| rest.split_once('"').unwrap().0)
            .collect()
    }

    #[test]
    fn authored_renders_sorted_nested_sections_and_safe_links() {
        let html = render_authored(&rich_snapshot());
        assert!(html.find("A/repo").unwrap() < html.find("z/repo").unwrap());
        assert!(html.find("Parent &lt;fix&gt;").unwrap() < html.find("title 2").unwrap());
        assert!(html.contains("<details class=\"pr-section checks\" data-state-key="));
        assert!(html.contains("<details class=\"pr-section reviewers\" data-state-key="));
        assert!(html.contains("<details class=\"pr-section comments\" data-state-key="));
        assert!(html.contains("<details class=\"pr-section stacked\" data-state-key="));
        assert!(html.contains("green — 1/1 required"));
        assert!(html.contains(
            "<span class=\"state ok\" aria-hidden=\"true\">✓</span><span class=\"check-state-label\">success</span>"
        ));
        assert!(html.contains(
            "<span class=\"state bad\" aria-hidden=\"true\">✗</span><span class=\"check-state-label\">failure</span>"
        ));
        assert!(
            html.find(" optional</a>").unwrap() < html.find(" required &amp; good <span").unwrap()
        );
        assert_eq!(html.matches("class=\"required-marker\"").count(), 1);
        assert!(html.contains("aria-label=\"required check\">◆</span>"));
        assert!(!html.contains("(not required)"));
        assert!(html.contains("changes requested (reviewed)"));
        assert!(html.contains("approved [req]"));
        assert!(html.contains("outdated"));
        assert!(html.contains("href=\"https://checks.test/?x=1&amp;y=2\""));
        assert!(html.contains("href=\"https://example.test/?a=1&amp;b=&quot;two&quot;\""));
        assert_eq!(
            html.matches("href=\"https").count(),
            html.matches("target=\"_blank\" rel=\"noopener noreferrer\"")
                .count()
        );
        assert!(!html.contains("href=\"/\" target=\"_blank\""));
        assert!(!html.contains("alice<script>"));
        assert!(!html.contains("eve<img>"));
        assert!(html.contains("say &lt;hello&gt; &amp; goodbye"));
    }

    #[test]
    fn both_pages_render_refresh_and_loading_disables_it() {
        let ready = rich_snapshot();
        let authored = render_authored(&ready);
        let merged = render_merged(&ready, Utc::now());
        assert!(authored.contains(
            "method=\"post\" action=\"/refresh?return=%2F\"><button type=\"submit\">Refresh"
        ));
        assert!(merged.contains(
            "method=\"post\" action=\"/refresh?return=%2Fmerged\"><button type=\"submit\">Refresh"
        ));

        let loading = render_authored(&WebSnapshot::default());
        assert!(loading.contains("<button type=\"submit\" disabled"));
        assert!(loading.contains("aria-busy=\"true\""));
        assert!(loading.contains("Refreshing…"));
        assert!(loading.contains("window.location.reload()"));
    }

    #[test]
    fn section_state_keys_are_unique_semantic_stable_and_escaped() {
        let first = render_authored(&rich_snapshot());
        let first_keys = state_keys(&first);
        let unique: std::collections::BTreeSet<_> = first_keys.iter().copied().collect();
        assert_eq!(first_keys.len(), unique.len());
        assert!(unique.contains("repo:z/repo:pr:1:section:checks"));
        assert!(unique.contains("repo:z/repo:pr:1:section:stacked"));

        let mut changed = rich_snapshot();
        changed.authored.reverse();
        changed.authored[0].title = "new title and content".to_string();
        for pr in &mut changed.authored {
            pr.updated_at += chrono::Duration::seconds(1_000);
        }
        let second = render_authored(&changed);
        let second_keys: std::collections::BTreeSet<_> = state_keys(&second).into_iter().collect();
        assert_eq!(unique, second_keys);

        let mut unsafe_snapshot = rich_snapshot();
        unsafe_snapshot.authored[0].repo = "bad\" data-injected=\"yes".to_string();
        let escaped = render_authored(&unsafe_snapshot);
        assert!(escaped.contains("repo:bad&quot; data-injected=&quot;yes:pr:1"));
        assert!(!escaped.contains("data-state-key=\"repo:bad\" data-injected="));
    }

    #[test]
    fn browser_state_script_restores_explicit_open_and_closed_values() {
        assert!(SCRIPT.contains("if (stored !== null) details.open = stored === \"open\""));
        assert!(SCRIPT.contains("details.open ? \"open\" : \"closed\""));
        assert!(SCRIPT.contains("sessionStorage"));
    }

    #[test]
    fn status_and_empty_states_render() {
        let snapshot = WebSnapshot {
            error: Some("boom <bad>".to_string()),
            status: Some("warning: partial & stale".to_string()),
            loaded_at: Some(Local.timestamp_opt(100, 0).unwrap()),
            ..WebSnapshot::default()
        };
        let html = render_authored(&snapshot);
        assert!(html.contains("Loading GitHub data"));
        assert!(html.contains("Refresh failed: boom &lt;bad&gt;"));
        assert!(html.contains("warning: partial &amp; stale"));
        assert!(html.contains("Latest successful refresh"));
        assert!(html.contains("No open authored pull requests"));
    }

    #[test]
    fn refresh_error_keeps_last_good_data_visible() {
        let mut snapshot = rich_snapshot();
        snapshot.error = Some("network down".to_string());
        let html = render_authored(&snapshot);
        assert!(html.contains("Refresh failed: network down"));
        assert!(html.contains("Parent &lt;fix&gt;"));
        assert!(!html.contains("aria-busy=\"true\""));
    }

    #[test]
    fn merged_filters_orders_and_caps_like_me_view() {
        let now = Utc.timestamp_opt(10_000, 0).unwrap();
        let mut snapshot = WebSnapshot {
            viewer: "me".to_string(),
            ..WebSnapshot::default()
        };
        snapshot.reviewing.push(pr("r/x", 99, "bob", 0));
        for i in 0..15 {
            let author = if i == 3 {
                "outsider"
            } else if i % 2 == 0 {
                "me"
            } else {
                "bob"
            };
            let mut merged = pr("r/m", i + 1, author, 0);
            merged.merged_at = Some(Utc.timestamp_opt(9_900 - i as i64, 0).unwrap());
            snapshot.merged.push(merged);
        }
        let html = render_merged(&snapshot, now);
        assert_eq!(
            html.matches("<li><a href=").count(),
            report::MERGED_PANE_CAP
        );
        assert!(
            html.find("https://example.test/r/m/1").unwrap()
                < html.find("https://example.test/r/m/2").unwrap()
        );
        assert!(!html.contains("#4</span>"));
        assert!(html.contains("r/m"));
        assert!(html.contains("@bob"));
        assert_eq!(
            html.matches("href=\"https").count(),
            html.matches("target=\"_blank\" rel=\"noopener noreferrer\"")
                .count()
        );
    }

    #[test]
    fn unknown_route_is_plain_404() {
        let (tx, _rx) = mpsc::channel();
        let response = route(&WebSnapshot::default(), &request("GET", "/nope"), &tx);
        assert_eq!(response.status, "404 Not Found");
        assert_eq!(response.content_type, "text/plain; charset=utf-8");
        assert_eq!(response.body, "404 not found\n");
    }

    #[test]
    fn refresh_route_signals_and_redirects_to_originating_page() {
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            route(
                &WebSnapshot::default(),
                &request("POST", "/refresh?return=%2Fmerged"),
                &tx,
            )
        });
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Msg::WebRefresh { acknowledged } => acknowledged.send(()).unwrap(),
            _ => panic!("listener sent the wrong app message"),
        }
        let response = worker.join().unwrap();
        assert_eq!(response.status, "303 See Other");
        assert_eq!(response.headers, vec![("Location", "/merged".to_string())]);
    }

    #[test]
    fn refresh_route_rejects_cross_origin_and_unsupported_methods() {
        let (tx, rx) = mpsc::channel();
        let mut cross_origin = request("POST", "/refresh?return=%2F");
        cross_origin.origin = Some("http://evil.example".to_string());
        let response = route(&WebSnapshot::default(), &cross_origin, &tx);
        assert_eq!(response.status, "403 Forbidden");
        assert!(rx.try_recv().is_err());

        let response = route(
            &WebSnapshot::default(),
            &request("GET", "/refresh?return=%2F"),
            &tx,
        );
        assert_eq!(response.status, "405 Method Not Allowed");
        assert_eq!(response.headers, vec![("Allow", "POST".to_string())]);
    }

    #[test]
    fn ephemeral_listener_serves_published_snapshot() {
        let (tx, _rx) = mpsc::channel();
        let server = start("127.0.0.1:0", tx).unwrap();
        server.snapshots().publish(rich_snapshot());
        let mut stream = TcpStream::connect(server.local_addr()).unwrap();
        stream
            .write_all(b"GET /merged HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "unexpected response: {response:?}"
        );
        assert!(response.contains("Recently merged PRs"));
    }

    #[test]
    fn bind_failure_names_the_requested_address() {
        let (tx, _rx) = mpsc::channel();
        let server = start("127.0.0.1:0", tx.clone()).unwrap();
        let address = server.local_addr().to_string();
        let error = start(&address, tx).err().expect("second bind should fail");
        assert!(format!("{error:#}").contains(&address));
    }
}

//! HTTP API and the built-in label editor.
//!
//! GET  /                      editor page
//! GET  /api/status            printer status
//! POST /api/status/refresh    check the printer now
//! POST /api/preview           label JSON -> PNG
//! POST /api/print             label JSON -> queued job
//! GET  /api/jobs              recent jobs
//! GET  /api/jobs/{id}         one job
//! POST /api/jobs/{id}/cancel  stop a job (a printing job stops after the current label)
//! POST /api/jobs/{id}/retry   print the missing copies again (or all, if it finished)
//! POST /api/jobs/cancel       stop all jobs
//! GET  /api/icons?q=&limit=   search icons
//! GET  /api/emoji?q=&limit=   search emoji
//! GET  /assets/mdi.ttf        icon font (for showing icons in the browser)

use crate::icons;
use crate::service::{log, PrintRequest, Service};
use anyhow::Result;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;

const EDITOR_HTML: &str = include_str!("../web/editor.html");

type Shared = Arc<Service>;

pub async fn serve(svc: Shared) -> Result<()> {
    let api = Router::new()
        .route("/status", get(status))
        .route("/status/refresh", post(refresh))
        .route("/preview", post(preview))
        .route("/print", post(print))
        .route("/jobs", get(jobs))
        .route("/jobs/{id}", get(job))
        .route("/jobs/{id}/cancel", post(cancel))
        .route("/jobs/{id}/retry", post(retry))
        .route("/jobs/cancel", post(cancel_all))
        .route("/icons", get(search_icons))
        .route("/emoji", get(search_emoji))
        .layer(middleware::from_fn_with_state(svc.clone(), auth));
    let app = Router::new()
        .route("/", get(|| async { Html(EDITOR_HTML) }))
        .route("/assets/mdi.ttf", get(mdi_font))
        .nest("/api", api)
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(svc.clone());
    let listener = tokio::net::TcpListener::bind(&svc.config.http.listen).await?;
    log(format!("HTTP: listening on {}", svc.config.http.listen));
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

fn bad_request(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg.to_string() }))).into_response()
}

/// Require the configured token as `Authorization: Bearer <token>` or `?token=<token>`.
async fn auth(State(svc): State<Shared>, req: Request, next: Next) -> Response {
    let Some(token) = svc.config.http.token.as_deref() else {
        return next.run(req).await;
    };
    let header_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| t == token);
    let query_ok = req
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .any(|kv| kv.strip_prefix("token=") == Some(token));
    if header_ok || query_ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "missing or wrong access token" })))
            .into_response()
    }
}

fn parse_request(body: &Bytes) -> Result<PrintRequest, Response> {
    serde_json::from_slice(body).map_err(|e| bad_request(format!("invalid label JSON: {e}")))
}

async fn status(State(svc): State<Shared>) -> impl IntoResponse {
    Json(svc.status())
}

async fn refresh(State(svc): State<Shared>) -> impl IntoResponse {
    svc.request_refresh();
    (StatusCode::ACCEPTED, Json(svc.status()))
}

async fn preview(State(svc): State<Shared>, body: Bytes) -> Response {
    let req = match parse_request(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    let (label, tape, notes) = match svc.preview(&req) {
        Ok(v) => v,
        Err(e) => return bad_request(format!("{e:#}")),
    };
    let img = image::GrayImage::from_raw(label.width() as u32, label.height() as u32, label.to_gray())
        .expect("bitmap size matches");
    let mut png = Vec::new();
    if let Err(e) = img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let length_mm = label.width() as f32 * 25.4 / 180.0;
    let mut resp = (StatusCode::OK, [(header::CONTENT_TYPE, "image/png")], png).into_response();
    let h = resp.headers_mut();
    h.insert("x-label-length-mm", HeaderValue::from_str(&format!("{length_mm:.1}")).unwrap());
    h.insert("x-tape-mm", HeaderValue::from(tape.mm as u32));
    if !notes.is_empty() {
        let text: String = notes.join(" ").chars().filter(|c| c.is_ascii() && !c.is_ascii_control()).collect();
        if let Ok(v) = HeaderValue::from_str(&text) {
            h.insert("x-label-notes", v);
        }
    }
    resp
}

fn source(addr: &SocketAddr) -> String {
    let ip = addr.ip();
    let ip = match ip {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    format!("web {ip}")
}

/// One label (JSON object) or several (JSON array; they print as one printer job).
async fn print(State(svc): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, body: Bytes) -> Response {
    let src = source(&addr);
    if body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[') {
        let reqs: Vec<PrintRequest> = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => return bad_request(format!("invalid label JSON: {e}")),
        };
        return match svc.submit_all(reqs, &src) {
            Ok(jobs) => (StatusCode::ACCEPTED, Json(jobs)).into_response(),
            Err(e) => bad_request(format!("{e:#}")),
        };
    }
    let req = match parse_request(&body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    match svc.submit(req, &src) {
        Ok(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        Err(e) => bad_request(format!("{e:#}")),
    }
}

async fn jobs(State(svc): State<Shared>) -> impl IntoResponse {
    Json(svc.jobs())
}

async fn job(State(svc): State<Shared>, Path(id): Path<u64>) -> Response {
    match svc.job(id) {
        Some(j) => Json(j).into_response(),
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no such job" }))).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SearchParams {
    q: Option<String>,
    limit: Option<usize>,
}

async fn search_icons(Query(p): Query<SearchParams>) -> impl IntoResponse {
    Json(icons::search_icons(p.q.as_deref().unwrap_or(""), p.limit.unwrap_or(120).min(500)))
}

async fn search_emoji(Query(p): Query<SearchParams>) -> impl IntoResponse {
    Json(icons::search_emoji(p.q.as_deref().unwrap_or(""), p.limit.unwrap_or(160).min(2000)))
}

async fn mdi_font() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "font/ttf"), (header::CACHE_CONTROL, "public, max-age=86400")],
        icons::MDI_TTF,
    )
}

fn job_result(r: anyhow::Result<crate::service::JobInfo>) -> Response {
    match r {
        Ok(job) => Json(job).into_response(),
        Err(e) if e.to_string() == "no such job" => {
            (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "no such job" }))).into_response()
        }
        Err(e) => (StatusCode::CONFLICT, Json(serde_json::json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

async fn cancel(State(svc): State<Shared>, Path(id): Path<u64>) -> Response {
    job_result(svc.cancel(id))
}

async fn retry(State(svc): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(id): Path<u64>) -> Response {
    job_result(svc.retry(id, &source(&addr)))
}

async fn cancel_all(State(svc): State<Shared>) -> Response {
    Json(serde_json::json!({ "stopped": svc.cancel_all() })).into_response()
}

use actix_files::NamedFile;
use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use anyhow::{Result, anyhow};
use async_stream::stream;
use futures_util::StreamExt;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use tokio_stream::wrappers::IntervalStream;
use zim_rs::archive::Archive;
use zim_rs::search::{Query, Searcher};

/// Shared app state:
/// - processed_bytes: Atomic counter updated during upload/processing
/// - zim_path: Option<String> stored after upload finishes
#[derive(Clone)]
struct AppState {
    processed_bytes: Arc<AtomicU64>,
    zim_path: Arc<Mutex<Option<String>>>,
}

#[derive(Serialize, Deserialize)]
struct ZimResponse {
    content: String,
    file_path: String,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    file_path: String,
}

#[derive(Serialize)]
struct ArticleSummary {
    title: String,
}

/// helper: search via zim-rs (blocking; small wrapper)
fn search_zim_file(zim_file_path: &str, query: &str) -> Result<Vec<ArticleSummary>> {
    let zim = Archive::new(zim_file_path).map_err(|_| anyhow!("failed to open archive"))?;
    let mut searcher = Searcher::new(&zim).map_err(|_| anyhow!("failed to create searcher"))?;
    let query_obj = Query::new(query).map_err(|_| anyhow!("invalid query"))?;
    let search = searcher
        .search(&query_obj)
        .map_err(|_| anyhow!("search failed"))?;
    let result_set = search
        .get_results(0, 50)
        .map_err(|_| anyhow!("failed to get results"))?;

    Ok(result_set
        .into_iter()
        .filter_map(|r| r.ok())
        .map(|entry| ArticleSummary {
            title: entry.get_title(),
        })
        .collect())
}

/// helper: show a few entries as HTML (used by homepage after upload)
fn parse_zim_file_preview(file_path: &str) -> Result<String> {
    let zim = Archive::new(file_path).map_err(|_| anyhow!("failed to open zim file"))?;
    let mut html = String::new();
    let total = zim.get_articlecount();

    for idx in 0..std::cmp::min(10, total) {
        if let Ok(entry) = zim.get_entry_bytitle_index(idx) {
            let title = entry.get_title();
            // simple safe link (title may contain characters; client decode)
            html += &format!(
                r#"<li><a href="/article/{}" class="text-blue-500 underline">{}</a></li>"#,
                urlencoding::encode(&title),
                html_escape::encode_text(&title)
            );
        }
    }

    Ok(format!(
        r#"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Zim Viewer</title></head>
<body>
<h1>Sample Articles</h1>
<ul>{}</ul>
</body>
</html>"#,
        html
    ))
}

/// SSE endpoint: streams `data: ...\n\n` messages with processed bytes
#[get("/progress")]
async fn progress(state: web::Data<AppState>) -> impl Responder {
    // create a stream that yields current processed bytes every 500ms
    let processed = state.processed_bytes.clone();
    let s = stream! {
        loop {
            let p = processed.load(Ordering::Relaxed);
            // SSE `data:` lines must end with a blank line
            let line = format!("data: {{\"processed_bytes\":{}}}\n\n", p);
            yield Ok::<_, actix_web::Error>(web::Bytes::from(line));
            sleep(Duration::from_millis(500)).await;
        }
    };

    HttpResponse::Ok()
        .insert_header(("Content-Type", "text/event-stream"))
        .streaming(s)
}

/// Serve static index.html
#[get("/")]
async fn index() -> actix_web::Result<NamedFile> {
    Ok(NamedFile::open("static/index.html")?)
}

/// Serve article by title (title is URL-encoded on link)
#[get("/article/{title}")]
async fn article(path: web::Path<String>, state: web::Data<AppState>) -> impl Responder {
    let title_enc = path.into_inner();
    let title = match urlencoding::decode(&title_enc) {
        Ok(s) => s.into_owned(),
        Err(_) => title_enc,
    };

    let guard = state.zim_path.lock().unwrap();
    let path = match &*guard {
        Some(p) => p.clone(),
        None => return HttpResponse::BadRequest().body("No ZIM loaded"),
    };

    // Open archive (blocking) — quick attempt; for heavy work push to web::block if needed
    match Archive::new(&path) {
        Ok(zim) => {
            // try entry by title string
            match zim.get_entry_bytitle_str(&title) {
                Ok(entry) => {
                    if let Ok(item) = entry.get_item(true) {
                        if let Ok(blob) = item.get_data() {
                            let bytes = blob.data().as_ref();
                            // attempt to serve as utf-8 html; otherwise fallback to bytes
                            let content = String::from_utf8_lossy(bytes).into_owned();
                            return HttpResponse::Ok()
                                .content_type("text/html; charset=utf-8")
                                .body(content);
                        }
                    }
                    HttpResponse::NotFound().body("Article found but failed to read content")
                }
                Err(_) => HttpResponse::NotFound().body("Article not found"),
            }
        }
        Err(_) => HttpResponse::InternalServerError().body("Failed to open ZIM archive"),
    }
}

/// upload handler:
/// - streams incoming multipart
/// - writes chunks directly to a NamedTempFile (no full-RAM buffering)
/// - sends chunks to a background thread pool (rayon via par_bridge) for processing if needed
/// - updates `state.processed_bytes` as bytes are received/handled
#[post("/upload")]
async fn upload(
    mut payload: Multipart,
    state: web::Data<AppState>,
) -> Result<web::Json<ZimResponse>, actix_web::Error> {
    // temporary file to save uploaded archive
    let mut file =
        NamedTempFile::new().map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    let path_buf = file.path().to_owned();

    // channel to pass chunks to processing background
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);

    // background thread: consume channel in parallel
    std::thread::spawn(move || {
        // rx.into_iter() yields Vec<u8>; use par_bridge to parallelize processing
        rx.into_iter().par_bridge().for_each(|chunk| {
            // TODO: replace this with actual CPU work (parsing entries etc.)
            // e.g., parse the chunk, index, upload part, etc.
            // Here we just simulate a small CPU job:
            let _len = chunk.len();
            // ... CPU work ...
        });
    });

    // Stream upload: for each chunk, write to file and update processed_bytes and send to tx
    while let Some(item) = payload.next().await {
        let mut field = item?;
        while let Some(chunk_res) = field.next().await {
            let chunk = chunk_res?;
            // write to temp file
            file.write(&chunk)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            // update shared processed counter
            state
                .processed_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            // send chunk for background processing (ignore send error if receiver dropped)
            let _ = tx.send(chunk.to_vec());
        }
    }

    // close file (persist)
    let persisted_path = path_buf.to_string_lossy().into_owned();

    // save path into state
    {
        let mut guard = state.zim_path.lock().unwrap();
        *guard = Some(persisted_path.clone());
    }

    // optional: reset processed_bytes to 0 (or keep cumulative)
    // state.processed_bytes.store(0, Ordering::Relaxed);

    // build preview HTML (small)
    let preview = match parse_zim_file_preview(&persisted_path) {
        Ok(h) => h,
        Err(e) => format!("Uploaded, but preview failed: {}", e),
    };

    Ok(web::Json(ZimResponse {
        content: preview,
        file_path: persisted_path,
    }))
}

/// search endpoint example using web::block to avoid blocking async reactor
#[post("/search")]
async fn search_articles(req: web::Json<SearchRequest>) -> impl Responder {
    let file_path = req.file_path.clone();
    let query = req.query.clone();

    // run blocking search in threadpool
    match web::block(move || search_zim_file(&file_path, &query)).await {
        Ok(Ok(results)) => HttpResponse::Ok().json(results),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(e.to_string()),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // initial state
    let state = AppState {
        processed_bytes: Arc::new(AtomicU64::new(0)),
        zim_path: Arc::new(Mutex::new(None)),
    };

    println!("Server running on http://127.0.0.1:8080");

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .service(index)
            .service(progress)
            .service(upload)
            .service(article)
            .service(search_articles)
            // serve static files under /static (keeps your index.html)
            .service(actix_files::Files::new("/static", "./static").index_file("index.html"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

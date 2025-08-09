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

fn search_zim_file(zim_file_path: &str, query: &str) -> Result<Vec<ArticleSummary>> {
    println!(
        "Searching ZIM file '{}' for query '{}'",
        zim_file_path, query
    );
    let zim = Archive::new(zim_file_path).map_err(|_| anyhow!("failed to open archive"))?;
    let mut searcher = Searcher::new(&zim).map_err(|_| anyhow!("failed to create searcher"))?;

    let query_obj = Query::new(query).map_err(|_| anyhow!("invalid query"))?;
    let search = searcher
        .search(&query_obj)
        .map_err(|_| anyhow!("search failed"))?;
    let mut result_vec: Vec<_> = search
        .get_results(0, 50)
        .map_err(|_| anyhow!("failed to get results"))?
        .into_iter()
        .collect();

    if result_vec.is_empty() {
        println!("No results found for '{}', trying lowercase search", query);
        let lower_query = query.to_lowercase();
        let query_obj = Query::new(&lower_query).map_err(|_| anyhow!("invalid query"))?;
        let search = searcher
            .search(&query_obj)
            .map_err(|_| anyhow!("search failed"))?;
        result_vec = search
            .get_results(0, 50)
            .map_err(|_| anyhow!("failed to get results"))?
            .into_iter()
            .collect();
    }

    let results: Vec<ArticleSummary> = result_vec
        .into_iter()
        .filter_map(|r| match r {
            Ok(entry) => Some(ArticleSummary {
                title: entry.get_title(),
            }),
            Err(e) => {
                eprintln!("Search entry error: {:?}", e);
                None
            }
        })
        .collect();

    println!("Search returned {} results", results.len());
    Ok(results)
}

fn parse_zim_file_preview(file_path: &str) -> Result<String> {
    let zim = Archive::new(file_path).map_err(|_| anyhow!("failed to open zim file"))?;
    let mut html = String::new();
    let total = zim.get_articlecount();

    for idx in 0..std::cmp::min(10, total) {
        if let Ok(entry) = zim.get_entry_bytitle_index(idx) {
            let title = entry.get_title();
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

#[get("/progress")]
async fn progress(state: web::Data<AppState>) -> impl Responder {
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

#[get("/")]
async fn index() -> actix_web::Result<NamedFile> {
    Ok(NamedFile::open("static/index.html")?)
}

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

    match Archive::new(&path) {
        Ok(zim) => {
            // try entry by title string
            match zim.get_entry_bytitle_str(&title) {
                Ok(entry) => {
                    if let Ok(item) = entry.get_item(true) {
                        if let Ok(blob) = item.get_data() {
                            let bytes = blob.data().as_ref();
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
        rx.into_iter().par_bridge().for_each(|chunk| {
            // TODO: CPU work
            // e.g., parse the chunk, index, upload part, etc.
            let _len = chunk.len();
        });
    });

    while let Some(item) = payload.next().await {
        let mut field = item?;
        while let Some(chunk_res) = field.next().await {
            let chunk = chunk_res?;
            // write to temp file
            file.write(&chunk)
                .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
            state
                .processed_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
            let _ = tx.send(chunk.to_vec());
        }
    }

    let persisted_path = path_buf.to_string_lossy().into_owned();

    // save path into state
    {
        let mut guard = state.zim_path.lock().unwrap();
        *guard = Some(persisted_path.clone());
    }

    // build preview HTML
    let preview = match parse_zim_file_preview(&persisted_path) {
        Ok(h) => h,
        Err(e) => format!("Uploaded, but preview failed: {}", e),
    };

    Ok(web::Json(ZimResponse {
        content: preview,
        file_path: persisted_path,
    }))
}

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
            .service(actix_files::Files::new("/static", "./static").index_file("index.html"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

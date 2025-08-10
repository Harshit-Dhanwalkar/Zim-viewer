use actix_files::NamedFile;
use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use anyhow::{Result, anyhow};
use async_stream::stream;
use futures_util::StreamExt;
use hex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use zim_rs::archive::Archive;
use zim_rs::search::{Query, Searcher};

#[derive(Clone)]
struct AppState {
    processed_bytes: Arc<AtomicU64>,
    uploaded_files: Arc<Mutex<HashMap<String, PathBuf>>>,
    current_zim_path: Arc<Mutex<Option<PathBuf>>>,
    file_cache: Arc<Mutex<HashMap<String, PathBuf>>>,
}

#[derive(Serialize, Deserialize)]
struct ZimResponse {
    message: String,
    file_metadata: AppMetadata,
}

#[derive(Serialize, Deserialize, Clone)]
struct AppMetadata {
    original_file_name: String,
    persisted_file_path: PathBuf,
    article_count: u64,
}

#[derive(Deserialize)]
struct SearchRequest {
    query: String,
    file_path: PathBuf,
}

#[derive(Deserialize)]
struct BrowseRequest {
    file_path: PathBuf,
}

#[derive(Serialize)]
struct ArticleSummary {
    title: String,
}

fn search_zim_file(zim_file_path: &Path, query: &str) -> Result<Vec<ArticleSummary>> {
    println!(
        "Searching ZIM file '{}' for query '{}'",
        zim_file_path.display(),
        query
    );
    let zim = Archive::new(zim_file_path.to_str().unwrap())
        .map_err(|e| anyhow!("Failed to open archive: {:?}", e))?;
    let mut searcher =
        Searcher::new(&zim).map_err(|e| anyhow!("Failed to create searcher: {:?}", e))?;

    let query_obj = Query::new(query).map_err(|e| anyhow!("Invalid query: {:?}", e))?;
    let search = searcher
        .search(&query_obj)
        .map_err(|e| anyhow!("Search failed: {:?}", e))?;
    let mut result_vec: Vec<_> = search
        .get_results(0, 50)
        .map_err(|e| anyhow!("Failed to get results: {:?}", e))?
        .into_iter()
        .collect();

    if result_vec.is_empty() {
        println!("No results found for '{}', trying lowercase search", query);
        let lower_query = query.to_lowercase();
        let query_obj = Query::new(&lower_query).map_err(|e| anyhow!("Invalid query: {:?}", e))?;
        let search = searcher
            .search(&query_obj)
            .map_err(|e| anyhow!("Search failed: {:?}", e))?;
        result_vec = search
            .get_results(0, 50)
            .map_err(|e| anyhow!("Failed to get results: {:?}", e))?
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

fn get_all_articles(file_path: &Path) -> Result<Vec<ArticleSummary>> {
    let zim = Archive::new(file_path.to_str().unwrap())
        .map_err(|e| anyhow!("failed to open zim file: {:?}", e))?;
    let total = zim.get_articlecount();
    let mut articles = Vec::new();
    for idx in 0..total {
        if let Ok(entry) = zim.get_entry_bytitle_index(idx) {
            if let Ok(item) = entry.get_item(false) {
                if let Ok(mimetype) = item.get_mimetype() {
                    if mimetype.starts_with("text/html") {
                        let title = entry.get_title();
                        articles.push(ArticleSummary { title });
                    }
                }
            }
        }
    }
    Ok(articles)
}

#[get("/progress")]
async fn progress(state: web::Data<AppState>) -> impl Responder {
    let processed = state.processed_bytes.clone();
    let s = stream! {
        loop {
            let p = processed.load(Ordering::Relaxed);
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

    let guard = state.current_zim_path.lock().unwrap();
    let path = match &*guard {
        Some(p) => p.clone(),
        None => return HttpResponse::BadRequest().body("No ZIM loaded"),
    };

    let path_str = match path.to_str() {
        Some(s) => s,
        None => return HttpResponse::InternalServerError().body("Invalid ZIM file path"),
    };

    match Archive::new(path_str) {
        Ok(zim) => match zim.get_entry_bytitle_str(&title) {
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
        },
        Err(_) => HttpResponse::InternalServerError().body("Failed to open ZIM archive"),
    }
}

#[post("/upload")]
async fn upload(
    mut payload: Multipart,
    state: web::Data<AppState>,
) -> Result<web::Json<ZimResponse>, actix_web::Error> {
    state.processed_bytes.store(0, Ordering::Relaxed);
    let uploads_dir = Path::new("./uploads");
    if !uploads_dir.exists() {
        fs::create_dir(uploads_dir).map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    }

    let mut original_file_name: Option<String> = None;
    let mut hasher = Sha256::new();
    let mut file_data = Vec::new();

    // Read all chunks and hash them
    while let Some(item) = payload.next().await {
        let mut field = item?;
        if original_file_name.is_none() {
            if let Some(filename) = field.content_disposition().get_filename() {
                original_file_name = Some(filename.to_string());
            }
        }
        while let Some(chunk_res) = field.next().await {
            let chunk = chunk_res?;
            hasher.update(&chunk);
            file_data.extend_from_slice(&chunk);
            state
                .processed_bytes
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }

    let original_file_name = original_file_name.unwrap_or_else(|| "unknown.zim".to_string());
    let hash = hex::encode(hasher.finalize());
    let new_filename = format!("{}.zim", hash);
    let persisted_path = uploads_dir.join(&new_filename);

    let article_count;

    // Check if the file already exists in the cache
    let mut file_cache_guard = state.file_cache.lock().unwrap();
    if let Some(cached_path) = file_cache_guard.get(&hash) {
        let cached_path_str = cached_path
            .to_str()
            .ok_or_else(|| anyhow!("Invalid cached file path"))
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
        article_count = match Archive::new(cached_path_str) {
            Ok(zim) => zim.get_articlecount() as u64,
            Err(e) => {
                eprintln!("Failed to open cached ZIM archive: {:?}", e);
                0
            }
        };

        let mut path_guard = state.current_zim_path.lock().unwrap();
        *path_guard = Some(cached_path.clone());

        return Ok(web::Json(ZimResponse {
            message: "File found in cache, no re-upload needed.".to_string(),
            file_metadata: AppMetadata {
                original_file_name,
                persisted_file_path: cached_path.clone(),
                article_count,
            },
        }));
    }

    let mut file =
        File::create(&persisted_path).map_err(|e| actix_web::error::ErrorInternalServerError(e))?;
    file.write_all(&file_data)
        .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

    article_count = match Archive::new(persisted_path.to_str().unwrap()) {
        Ok(zim) => zim.get_articlecount() as u64,
        Err(e) => {
            eprintln!("Failed to open new ZIM archive to get metadata: {:?}", e);
            0
        }
    };

    {
        let mut files_guard = state.uploaded_files.lock().unwrap();
        files_guard.insert(original_file_name.clone(), persisted_path.clone());
        let mut path_guard = state.current_zim_path.lock().unwrap();
        *path_guard = Some(persisted_path.clone());
    }

    file_cache_guard.insert(hash, persisted_path.clone());

    Ok(web::Json(ZimResponse {
        message: "File uploaded successfully".to_string(),
        file_metadata: AppMetadata {
            original_file_name,
            persisted_file_path: persisted_path,
            article_count,
        },
    }))
}

#[post("/search")]
async fn search_articles(req: web::Json<SearchRequest>) -> impl Responder {
    let file_path = req.file_path.clone();
    let query = req.query.clone();

    match web::block(move || search_zim_file(&file_path, &query)).await {
        Ok(Ok(results)) => HttpResponse::Ok().json(results),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(e.to_string()),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[post("/browse")]
async fn browse_articles(req: web::Json<BrowseRequest>) -> impl Responder {
    let file_path = req.file_path.clone();
    match web::block(move || get_all_articles(&file_path)).await {
        Ok(Ok(articles)) => HttpResponse::Ok().json(articles),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(e.to_string()),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[post("/clean_cache")]
async fn clean_cache(state: web::Data<AppState>) -> impl Responder {
    let uploads_dir = Path::new("./uploads");
    if uploads_dir.exists() {
        match fs::remove_dir_all(uploads_dir) {
            Ok(_) => {
                // Clear the state after deleting the directory
                let mut files_guard = state.uploaded_files.lock().unwrap();
                files_guard.clear();
                let mut path_guard = state.current_zim_path.lock().unwrap();
                *path_guard = None;
                // Clear the file cache
                let mut cache_guard = state.file_cache.lock().unwrap();
                cache_guard.clear();
                HttpResponse::Ok().body("Cache cleaned successfully")
            }
            Err(e) => {
                HttpResponse::InternalServerError().body(format!("Failed to clean cache: {}", e))
            }
        }
    } else {
        HttpResponse::Ok().body("Cache directory not found, nothing to clean")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let uploads_dir = Path::new("./uploads");
    if !uploads_dir.exists() {
        fs::create_dir(uploads_dir)?;
    }

    // Load existing files into the cache on startup
    let mut file_cache = HashMap::new();
    if uploads_dir.exists() {
        for entry in fs::read_dir(uploads_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let file_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !file_name.is_empty() {
                    file_cache.insert(file_name.to_string(), path);
                }
            }
        }
    }

    let state = AppState {
        processed_bytes: Arc::new(AtomicU64::new(0)),
        uploaded_files: Arc::new(Mutex::new(HashMap::new())),
        current_zim_path: Arc::new(Mutex::new(None)),
        file_cache: Arc::new(Mutex::new(file_cache)),
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
            .service(browse_articles)
            .service(clean_cache)
            .service(actix_files::Files::new("/", "./static").index_file("index.html"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

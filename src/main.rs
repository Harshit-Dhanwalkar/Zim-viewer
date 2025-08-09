use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use anyhow::{Result, anyhow};
use futures_util::StreamExt as _;
use indicatif::ProgressBar;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tempfile::NamedTempFile;
use zim_rs::archive::Archive;
use zim_rs::search::{Query, Searcher};

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

fn parse_zim_file(file_path: &str) -> Result<String> {
    let zim = Archive::new(file_path).map_err(|_| anyhow!("failed to open zim file"))?;
    let mut html = String::new();
    let total_articles = zim.get_articlecount();

    for idx in 0..std::cmp::min(5, total_articles) {
        match zim.get_entry_bytitle_index(idx) {
            Ok(entry) => {
                let title = entry.get_title();
                html += &format!("<h3 class=\"text-xl font-semibold mt-4\">{}</h3>", title);

                match entry.get_item(true) {
                    Ok(item) => match item.get_data() {
                        Ok(blob) => {
                            let bytes: &[u8] = blob.data().as_ref();
                            let content_len = bytes.len();
                            let content_str = String::from_utf8_lossy(bytes);

                            html += &format!(
                                "<p class=\"text-gray-600\">Article content size: {} bytes</p>",
                                content_len
                            );
                            html += &format!(
                                "<pre class=\"bg-gray-100 p-2 rounded-md overflow-x-auto text-sm\">{}</pre>",
                                &content_str[..std::cmp::min(100, content_str.len())]
                            );
                        }
                        Err(_) => html += "<p class=\"text-red-500\">Error reading blob data</p>",
                    },
                    Err(_) => html += "<p class=\"text-red-500\">No item for this entry</p>",
                }
            }
            Err(_) => {
                html += &format!(
                    "<h3 class=\"text-xl font-semibold mt-4 text-red-500\">Error getting entry at index {}</h3>",
                    idx
                )
            }
        }
    }

    Ok(format!(
        r#"<div class="p-6"><h2 class="text-2xl font-bold mb-4">Sample Articles</h2>{}</div>"#,
        html
    ))
}

fn process_chunks_in_parallel(chunks: Vec<String>) {
    let pb = ProgressBar::new(chunks.len() as u64);
    chunks.par_iter().for_each(|chunk| {
        process_and_upload_sync(chunk);
        pb.inc(1);
    });
    pb.finish();
}

fn process_and_upload_sync(chunk: &str) {
    // TODO: Implement actual CPU-bound processing + upload
    println!("Processing chunk: {}", chunk);
}

#[post("/search")]
async fn search_articles(req: web::Json<SearchRequest>) -> impl Responder {
    let query = req.query.clone();
    let file_path = req.file_path.clone();

    match web::block(move || search_zim_file(&file_path, &query)).await {
        Ok(Ok(results)) => HttpResponse::Ok().json(results),
        Ok(Err(e)) => HttpResponse::InternalServerError().body(e.to_string()),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[post("/upload")]
async fn upload(mut payload: Multipart) -> Result<String, actix_web::Error> {
    let mut file = NamedTempFile::new().unwrap();

    while let Some(field) = payload.next().await {
        let mut field = field?;
        while let Some(chunk) = field.next().await {
            let data = chunk?;
            file.write_all(&data)?;
        }
    }

    let path = file.into_temp_path();
    Ok(format!("{}", path.display()))
}

#[get("/")]
async fn index() -> impl Responder {
    actix_files::NamedFile::open_async("./static/index.html")
        .await
        .unwrap()
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    println!("Server running on http://127.0.0.1:8080");
    HttpServer::new(|| {
        App::new()
            .service(index)
            .service(upload)
            .service(search_articles)
            .service(actix_files::Files::new("/static", "./static"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

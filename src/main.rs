use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use futures_util::StreamExt as _;
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tempfile::{NamedTempFile, TempPath};
use zim_rs::archive::Archive;
use zim_rs::entry::Entry;
use zim_rs::search::{Query, Searcher};

#[derive(Serialize, Deserialize)]
struct ZimResponse {
    content: String,
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

fn search_zim_file(
    zim_file_path: &str,
    query: &str,
) -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    let zim = Archive::new(zim_file_path).map_err(|_| "Failed to open ZIM archive")?;
    let mut searcher = Searcher::new(&zim).map_err(|_| "Failed to create Searcher")?;
    let query_obj = Query::new(query).map_err(|_| "Failed to create Query")?;
    let search = searcher.search(&query_obj).map_err(|_| "Search failed")?;
    let result_set = search
        .get_results(0, 50)
        .map_err(|_| "Failed to get results")?;
    Ok(result_set.into_iter().filter_map(|r| r.ok()).collect())
}

async fn parse_zim_file(
    bytes: web::Bytes,
) -> Result<(String, TempPath), Box<dyn std::error::Error>> {
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(&bytes)?;
    let temp_path = temp_file.into_temp_path();

    let zim =
        Archive::new(temp_path.to_str().unwrap()).map_err(|_| "Failed to open ZIM archive")?;
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
                            // Handle blob.data() regardless of return type
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

    Ok((
        format!(
            r#"<div class="p-6"><h2 class="text-2xl font-bold mb-4">Sample Articles</h2>{}</div>"#,
            html
        ),
        temp_path,
    ))
}

#[post("/search")]
async fn search_articles(req: web::Json<SearchRequest>) -> impl Responder {
    match search_zim_file(&req.file_path, &req.query) {
        Ok(entries) => {
            let article_summaries: Vec<_> = entries
                .iter()
                .map(|e| ArticleSummary {
                    title: e.get_title().to_string(),
                })
                .collect();
            HttpResponse::Ok().json(article_summaries)
        }
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
}

#[post("/upload")]
async fn upload(mut payload: Multipart) -> actix_web::Result<String> {
    let mut file = NamedTempFile::new().unwrap();

    while let Some(Ok(mut field)) = payload.next().await {
        while let Some(Ok(chunk)) = field.next().await {
            file.write_all(&chunk)?;
        }
    }

    let path = file.into_temp_path();
    Ok(format!("File saved at {:?}", path))
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
            .service(upload_file)
            .service(search_articles)
            .service(actix_files::Files::new("/static", "./static"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

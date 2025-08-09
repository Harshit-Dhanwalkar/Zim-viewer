use actix_multipart::Multipart;
use actix_web::{App, HttpResponse, HttpServer, Responder, get, post, web};
use futures_util::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::Write;
use tempfile::NamedTempFile;
use zim_rs::archive::Archive;

#[derive(Serialize, Deserialize)]
struct ZimResponse {
    content: String,
}

async fn parse_zim_file(bytes: web::Bytes) -> Result<String, Box<dyn std::error::Error>> {
    let mut temp_file = NamedTempFile::new()?;
    temp_file.write_all(&bytes)?;

    let zim = Archive::new(temp_file.path().to_str().unwrap())
        .map_err(|_| "Failed to open ZIM archive")?;

    let mut html = String::new();
    let total_articles = zim.get_articlecount();

    for idx in 0..std::cmp::min(5, total_articles) {
        match zim.get_entry_bytitle_index(idx) {
            Ok(entry) => {
                let title = entry.get_title();
                html += &format!("<h3 class=\"text-xl font-semibold mt-4\">{}</h3>", title);

                if let Ok(item) = entry.get_item(true) {
                    if let Ok(blob) = item.get_data() {
                        let bytes = blob.data();
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
                    } else {
                        html += "<p class=\"text-red-500\">Error reading blob data</p>";
                    }
                } else {
                    html += "<p class=\"text-red-500\">No item for this entry</p>";
                }
            }
            Err(_) => {
                html += &format!(
                    "<h3 class=\"text-xl font-semibold mt-4 text-red-500\">Error getting entry at index {}</h3>",
                    idx
                );
            }
        }
    }

    Ok(format!(
        r#"<div class="p-6"><h2 class="text-2xl font-bold mb-4">Sample Articles</h2>{}</div>"#,
        html
    ))
}

#[post("/upload")]
async fn upload_file(mut payload: Multipart) -> impl Responder {
    let mut bytes = web::BytesMut::new();
    let mut file_found = false;

    while let Some(item) = payload.next().await {
        let mut field = match item {
            Ok(f) => f,
            Err(_) => return HttpResponse::InternalServerError().body("Multipart parsing error"),
        };

        if field.name() == "zim_file" {
            file_found = true;
            while let Some(chunk) = field.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(_) => {
                        return HttpResponse::InternalServerError().body("Error reading chunk");
                    }
                };
                bytes.extend_from_slice(&chunk);
            }
        }
    }

    if !file_found {
        return HttpResponse::BadRequest().body("No 'zim_file' uploaded");
    }

    match parse_zim_file(web::Bytes::from(bytes)).await {
        Ok(content) => HttpResponse::Ok().json(ZimResponse { content }),
        Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
    }
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
            .service(actix_files::Files::new("/static", "./static"))
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await
}

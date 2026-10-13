//! File uploads.
//!
//! ```text
//! cargo run --example upload
//!
//! curl -F "caption=holiday" -F "photo=@./photo.jpg" http://localhost:19012/upload
//! curl -F "files=@./a.txt" -F "files=@./b.txt"    http://localhost:19012/batch
//! ```
//!
//! Files are streamed: the body is never buffered whole, and anything past
//! `web.server.maxUploadMemory` (1 MiB by default) goes to a temporary file that the
//! framework deletes once the request is done with it.

use async_trait::async_trait;
use rust_web_frame as wf;
use rust_web_frame::config::load_auto_config;
use rust_web_frame::core::{Context, Rest};
use rust_web_frame::web::{Error, Message, Request, Result, handler};
use serde_json::json;

const UPLOAD_DIR: &str = "./uploads";

#[derive(Default)]
struct UploadController;

impl UploadController {
    /// A single file plus a text field.
    async fn one(req: Request) -> Result<Message> {
        let form = req.multipart().await?;

        let caption = form.text_or("caption", "(untitled)").to_owned();
        let photo = form
            .file("photo")
            .ok_or_else(|| Error::bad_request().detail("a `photo` file is required"))?;

        // `copy_to_dir` sanitises the client-supplied name, so `../../etc/passwd`
        // becomes `passwd` inside the upload directory.
        let saved = photo.copy_to_dir(UPLOAD_DIR).await?;

        Ok(Message::data(json!({
            "caption": caption,
            "field": photo.name(),
            "fileName": photo.sanitised_file_name(),
            "contentType": photo.content_type(),
            "size": photo.size(),
            "spilledToDisk": !photo.is_in_memory(),
            "savedAs": saved.display().to_string(),
        })))
    }

    /// Several files under one field name.
    async fn batch(req: Request) -> Result<Message> {
        let form = req.multipart().await?;
        let files = form.files("files");
        if files.is_empty() {
            return Err(Error::bad_request().detail("at least one `files` upload is required"));
        }

        let mut saved = Vec::with_capacity(files.len());
        for file in files {
            let path = file.copy_to_dir(UPLOAD_DIR).await?;
            saved.push(json!({
                "fileName": file.sanitised_file_name(),
                "size": file.size(),
                "path": path.display().to_string(),
            }));
        }

        Ok(Message::data(json!({
            "count": saved.len(),
            "files": saved,
            "note": form.text("note"),
        })))
    }

    /// Reads the content instead of saving it.
    async fn inspect(req: Request) -> Result<Message> {
        let form = req.multipart().await?;
        let file = form
            .file("doc")
            .ok_or_else(|| Error::bad_request().detail("a `doc` file is required"))?;

        let bytes = file.bytes().await?;
        Ok(Message::data(json!({
            "size": bytes.len(),
            "preview": String::from_utf8_lossy(&bytes[..bytes.len().min(64)]),
        })))
    }
}

#[async_trait]
impl Rest for UploadController {
    async fn init(&self, ctx: &Context) -> Result<()> {
        ctx.post("/upload", handler(Self::one));
        ctx.post("/batch", handler(Self::batch));
        ctx.post("/inspect", handler(Self::inspect));
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut config = load_auto_config()?;
    config.set("web.server.port", json!(19012));
    // Raise the body limit for real uploads. `-1` means unlimited.
    config.set("web.server.maxBodySize", json!(64 * 1024 * 1024));
    // Keep at most 4 MiB of each upload in memory; the rest goes to a temporary file.
    config.set("web.server.maxUploadMemory", json!(4 * 1024 * 1024));

    let mut builder = wf::Builder::new(config);
    builder.rest(UploadController);
    builder.build().run().await?;
    Ok(())
}

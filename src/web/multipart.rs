//! `multipart/form-data` parsing — file uploads.
//!
//! A multipart body is **streamed**, never fully buffered: each part is read chunk by
//! chunk and, once a file outgrows [`ServerConfig::max_upload_memory`], it spills to a
//! temporary file that is deleted when the [`UploadedFile`] is dropped. Memory use is
//! therefore bounded by the number of concurrent uploads, not by their size.
//!
//! ```no_run
//! use rust_web_frame::web::{Message, Request, Result};
//! use serde_json::json;
//!
//! async fn upload(req: Request) -> Result<Message> {
//!     let form = req.multipart().await?;
//!
//!     let caption = form.text("caption").unwrap_or_default().to_owned();
//!     let Some(avatar) = form.file("avatar") else {
//!         return Err(rust_web_frame::web::Error::bad_request().detail("avatar is required"));
//!     };
//!
//!     // `copy_to_dir` sanitises the client-supplied name before joining it.
//!     let saved = avatar.copy_to_dir("./uploads").await?;
//!     Ok(Message::data(json!({
//!         "caption": caption,
//!         "path": saved.display().to_string(),
//!         "size": avatar.size(),
//!     })))
//! }
//! ```
//!
//! [`ServerConfig::max_upload_memory`]: super::ServerConfig::max_upload_memory

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::AsyncWriteExt;

use super::error::{Error, Result};

/// How much of one uploaded file is held in memory before it spills to a temporary file.
pub const DEFAULT_MAX_UPLOAD_MEMORY: u64 = 1024 * 1024;

/// The limits a multipart body is parsed under.
#[derive(Debug, Clone, Copy)]
pub struct MultipartLimits {
    /// Maximum size of the whole body. `u64::MAX` means unlimited.
    pub max_body_size: u64,
    /// Bytes of one file held in memory before it spills to disk.
    pub max_upload_memory: u64,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        Self {
            max_body_size: u64::MAX,
            max_upload_memory: DEFAULT_MAX_UPLOAD_MEMORY,
        }
    }
}

/// A parsed `multipart/form-data` body: text fields and uploaded files, in the order the
/// client sent them.
///
/// A name may repeat — checkbox groups and multi-file inputs do — so every accessor comes
/// in a "first one" and an "all of them" flavour.
#[derive(Debug, Default)]
pub struct MultipartForm {
    fields: HashMap<String, Vec<String>>,
    files: HashMap<String, Vec<UploadedFile>>,
}

impl MultipartForm {
    /// The first value of a text field.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        self.fields.get(name)?.first().map(String::as_str)
    }

    /// The first value of a text field, or `default`.
    #[must_use]
    pub fn text_or<'a>(&'a self, name: &str, default: &'a str) -> &'a str {
        self.text(name).unwrap_or(default)
    }

    /// Every value of a repeated text field.
    #[must_use]
    pub fn texts(&self, name: &str) -> &[String] {
        self.fields.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Reports whether a text field was present.
    #[must_use]
    pub fn contains_text(&self, name: &str) -> bool {
        self.fields.contains_key(name)
    }

    /// Every text field name.
    pub fn text_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }

    /// The first file sent under `name`.
    #[must_use]
    pub fn file(&self, name: &str) -> Option<&UploadedFile> {
        self.files.get(name)?.first()
    }

    /// Every file sent under `name`.
    #[must_use]
    pub fn files(&self, name: &str) -> &[UploadedFile] {
        self.files.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Every file in the form, whatever its field name.
    pub fn all_files(&self) -> impl Iterator<Item = &UploadedFile> {
        self.files.values().flatten()
    }

    /// How many files the form carries.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// Reports whether the form carries at least one file.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty() && self.files.is_empty()
    }

    /// Inserts a text field.
    pub(crate) fn push_text(&mut self, name: String, value: String) {
        self.fields.entry(name).or_default().push(value);
    }

    /// Inserts a file.
    pub(crate) fn push_file(&mut self, name: String, file: UploadedFile) {
        self.files.entry(name).or_default().push(file);
    }
}

/// One file from a multipart body.
///
/// Small files are kept in memory; larger ones live in a temporary file that is removed
/// when this value is dropped. Either way [`bytes`](Self::bytes) and
/// [`copy_to`](Self::copy_to) work the same.
#[derive(Debug)]
pub struct UploadedFile {
    name: String,
    file_name: String,
    content_type: Option<String>,
    size: u64,
    content: Content,
}

#[derive(Debug)]
enum Content {
    Memory(Bytes),
    /// Kept alive for its drop: removing the file when the upload is no longer needed.
    Temp(tempfile::TempPath),
}

impl UploadedFile {
    /// The form field the file arrived under, e.g. `avatar`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file name the *client* supplied. Never trust it for a path — use
    /// [`sanitised_file_name`](Self::sanitised_file_name) or
    /// [`copy_to_dir`](Self::copy_to_dir).
    #[must_use]
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    /// The client-supplied file name reduced to a safe basename: no directory
    /// components, no `..`, no control characters, no leading dots.
    #[must_use]
    pub fn sanitised_file_name(&self) -> String {
        sanitise_file_name(&self.file_name)
    }

    /// The `Content-Type` the client declared.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Reports whether the content is still in memory.
    #[must_use]
    pub fn is_in_memory(&self) -> bool {
        matches!(self.content, Content::Memory(_))
    }

    /// The temporary file holding the content, when it spilled to disk.
    ///
    /// The file disappears when this value is dropped, so take a copy if you need it to
    /// outlive the request.
    #[must_use]
    pub fn temp_path(&self) -> Option<&Path> {
        match &self.content {
            Content::Memory(_) => None,
            Content::Temp(path) => Some(path),
        }
    }

    /// Reads the whole content into memory.
    ///
    /// # Errors
    ///
    /// Fails when a spilled file can no longer be read.
    pub async fn bytes(&self) -> Result<Bytes> {
        match &self.content {
            Content::Memory(bytes) => Ok(bytes.clone()),
            Content::Temp(path) => Ok(Bytes::from(tokio::fs::read(path).await?)),
        }
    }

    /// Writes the content to `destination`, creating parent directories as needed.
    ///
    /// Returns how many bytes were written.
    ///
    /// # Errors
    ///
    /// Fails when the destination cannot be created or written.
    pub async fn copy_to(&self, destination: impl AsRef<Path>) -> Result<u64> {
        let destination = destination.as_ref();
        if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        match &self.content {
            Content::Memory(bytes) => {
                tokio::fs::write(destination, bytes).await?;
                Ok(bytes.len() as u64)
            }
            Content::Temp(path) => {
                tokio::fs::copy(path, destination).await?;
                Ok(self.size)
            }
        }
    }

    /// Writes the content into `directory` under its
    /// [sanitised](Self::sanitised_file_name) file name, and returns the path it landed
    /// at.
    ///
    /// The name is derived from the client's, which is why it is sanitised first — a
    /// client that sends `../../etc/passwd` gets `passwd` inside `directory`
    /// regardless.
    ///
    /// # Errors
    ///
    /// Fails when the destination cannot be created or written.
    pub async fn copy_to_dir(&self, directory: impl AsRef<Path>) -> Result<PathBuf> {
        let destination = directory.as_ref().join(self.sanitised_file_name());
        self.copy_to(&destination).await?;
        Ok(destination)
    }
}

/// Reduces a client-supplied file name to a safe basename.
///
/// Strips everything up to the last separator of either flavour, drops `..`, control
/// characters and characters Windows reserves, and falls back to `upload` when nothing
/// usable is left.
#[must_use]
pub fn sanitise_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_start_matches('.');

    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .collect();

    let cleaned = cleaned.trim().to_owned();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return String::from("upload");
    }
    // Windows refuses these outright, whatever the extension.
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return format!("_{cleaned}");
    }
    cleaned
}

/// Parses a multipart body from any byte stream.
///
/// # Errors
///
/// Fails when the boundary is missing, the body is malformed, a part exceeds the
/// configured limits, or a temporary file cannot be created.
pub async fn parse<S, O, E>(
    stream: S,
    content_type: &str,
    limits: MultipartLimits,
) -> Result<MultipartForm>
where
    S: Stream<Item = std::result::Result<O, E>> + Send + 'static,
    O: Into<Bytes> + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    let boundary = multer::parse_boundary(content_type)?;

    let constraints = multer::Constraints::new().size_limit(
        multer::SizeLimit::new()
            .whole_stream(limits.max_body_size)
            .per_field(limits.max_body_size),
    );
    let mut multipart: multer::Multipart<'static> =
        multer::Multipart::with_constraints(stream, boundary, constraints);

    let mut form = MultipartForm::default();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        match field.file_name().map(str::to_owned) {
            None => form.push_text(name, field.text().await?),
            Some(file_name) => {
                let content_type = field.content_type().map(ToString::to_string);
                let file = collect_file(field, &name, file_name, content_type, limits).await?;
                form.push_file(name, file);
            }
        }
    }
    Ok(form)
}

/// Reads one file part, spilling to a temporary file once it outgrows the in-memory
/// budget.
async fn collect_file(
    mut field: multer::Field<'static>,
    name: &str,
    file_name: String,
    content_type: Option<String>,
    limits: MultipartLimits,
) -> Result<UploadedFile> {
    let mut memory = Vec::new();
    let mut spilled: Option<(tempfile::TempPath, tokio::fs::File)> = None;
    let mut size: u64 = 0;

    while let Some(chunk) = field.chunk().await? {
        size += chunk.len() as u64;

        match &mut spilled {
            Some((_, file)) => {
                file.write_all(&chunk).await?;
            }
            None if size > limits.max_upload_memory => {
                let temp = tempfile::Builder::new()
                    .prefix("rust-web-frame-upload-")
                    .tempfile()
                    .map_err(Error::from)?;
                let path = temp.into_temp_path();
                let mut file = tokio::fs::File::create(&path).await?;
                file.write_all(&memory).await?;
                memory = Vec::new();
                file.write_all(&chunk).await?;
                spilled = Some((path, file));
            }
            None => memory.extend_from_slice(&chunk),
        }
    }

    let content = match spilled {
        Some((path, mut file)) => {
            file.flush().await?;
            drop(file);
            Content::Temp(path)
        }
        None => Content::Memory(Bytes::from(memory)),
    };

    Ok(UploadedFile {
        name: name.to_owned(),
        file_name,
        content_type,
        size,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::once;

    fn multipart_body(boundary: &str, parts: &str) -> Bytes {
        Bytes::from(format!("--{boundary}\r\n{parts}\r\n--{boundary}--\r\n"))
    }

    fn content_type(boundary: &str) -> String {
        format!("multipart/form-data; boundary={boundary}")
    }

    fn stream_of(
        bytes: Bytes,
    ) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static {
        once(Ok(bytes))
    }

    #[tokio::test]
    async fn text_fields_and_files_are_both_parsed() {
        let boundary = "X-BOUNDARY";
        let body = multipart_body(
            boundary,
            "Content-Disposition: form-data; name=\"caption\"\r\n\r\n\
             a nice photo\r\n\
             --X-BOUNDARY\r\n\
             Content-Disposition: form-data; name=\"avatar\"; filename=\"me.png\"\r\n\
             Content-Type: image/png\r\n\r\n\
             PNGDATA",
        );

        let form = parse(
            stream_of(body),
            &content_type(boundary),
            MultipartLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(form.text("caption"), Some("a nice photo"));
        assert_eq!(form.file_count(), 1);

        let file = form.file("avatar").unwrap();
        assert_eq!(file.name(), "avatar");
        assert_eq!(file.file_name(), "me.png");
        assert_eq!(file.content_type(), Some("image/png"));
        assert_eq!(file.size(), 7);
        assert!(file.is_in_memory());
        assert_eq!(file.bytes().await.unwrap(), Bytes::from_static(b"PNGDATA"));
    }

    #[tokio::test]
    async fn repeated_names_keep_every_value() {
        let boundary = "B";
        let body = multipart_body(
            boundary,
            "Content-Disposition: form-data; name=\"tag\"\r\n\r\none\r\n\
             --B\r\n\
             Content-Disposition: form-data; name=\"tag\"\r\n\r\ntwo\r\n\
             --B\r\n\
             Content-Disposition: form-data; name=\"doc\"; filename=\"a.txt\"\r\n\r\nA\r\n\
             --B\r\n\
             Content-Disposition: form-data; name=\"doc\"; filename=\"b.txt\"\r\n\r\nB",
        );

        let form = parse(
            stream_of(body),
            &content_type(boundary),
            MultipartLimits::default(),
        )
        .await
        .unwrap();

        assert_eq!(form.texts("tag"), ["one", "two"]);
        assert_eq!(form.text("tag"), Some("one"));
        assert_eq!(form.files("doc").len(), 2);
        assert_eq!(form.all_files().count(), 2);
    }

    #[tokio::test]
    async fn a_large_file_spills_to_disk_and_still_reads_back() {
        let boundary = "BIG";
        let payload = vec![b'x'; 4096];
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"blob\"; filename=\"big.bin\"\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&payload);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let limits = MultipartLimits {
            max_body_size: u64::MAX,
            max_upload_memory: 1024,
        };
        let form = parse(
            stream_of(Bytes::from(body)),
            &content_type(boundary),
            limits,
        )
        .await
        .unwrap();

        let file = form.file("blob").unwrap();
        assert!(
            !file.is_in_memory(),
            "must have spilled to a temporary file"
        );
        assert_eq!(file.size(), 4096);

        let path = file.temp_path().unwrap().to_path_buf();
        assert!(path.exists());
        assert_eq!(file.bytes().await.unwrap().len(), 4096);

        // Dropping the form removes the temporary file.
        drop(form);
        assert!(!path.exists(), "the temporary file must be cleaned up");
    }

    #[tokio::test]
    async fn the_body_size_limit_is_enforced() {
        let boundary = "B";
        let body = multipart_body(
            boundary,
            "Content-Disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\n\r\n\
             0123456789012345678901234567890123456789",
        );

        let limits = MultipartLimits {
            max_body_size: 32,
            ..MultipartLimits::default()
        };
        let error = parse(stream_of(body), &content_type(boundary), limits)
            .await
            .unwrap_err();
        assert_eq!(error.code, 413);
    }

    #[tokio::test]
    async fn a_missing_boundary_is_a_bad_request() {
        let error = parse(
            stream_of(Bytes::from_static(b"whatever")),
            "multipart/form-data",
            MultipartLimits::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, 400);
    }

    #[test]
    fn file_names_are_sanitised_before_being_used_as_paths() {
        assert_eq!(sanitise_file_name("photo.png"), "photo.png");
        assert_eq!(sanitise_file_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitise_file_name(r"..\..\windows\system32\cmd.exe"),
            "cmd.exe"
        );
        assert_eq!(sanitise_file_name("/absolute/path.txt"), "path.txt");
        assert_eq!(sanitise_file_name("..."), "upload");
        assert_eq!(sanitise_file_name("   "), "upload");
        assert_eq!(sanitise_file_name(""), "upload");
        assert_eq!(sanitise_file_name("a\u{0}b.txt"), "ab.txt");
        assert_eq!(sanitise_file_name("nul"), "_nul");
        assert_eq!(sanitise_file_name("CON.txt"), "_CON.txt");
    }

    #[tokio::test]
    async fn copying_into_a_directory_never_escapes_it() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");

        let file = UploadedFile {
            name: String::from("doc"),
            file_name: String::from("../../escape.txt"),
            content_type: None,
            size: 5,
            content: Content::Memory(Bytes::from_static(b"hello")),
        };

        let saved = file.copy_to_dir(&uploads).await.unwrap();
        assert_eq!(saved, uploads.join("escape.txt"));
        assert!(saved.starts_with(&uploads));
        assert_eq!(tokio::fs::read(&saved).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn copy_to_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("a/b/c/report.csv");

        let file = UploadedFile {
            name: String::from("report"),
            file_name: String::from("report.csv"),
            content_type: Some(String::from("text/csv")),
            size: 3,
            content: Content::Memory(Bytes::from_static(b"a,b")),
        };

        assert_eq!(file.copy_to(&destination).await.unwrap(), 3);
        assert_eq!(tokio::fs::read(&destination).await.unwrap(), b"a,b");
    }
}

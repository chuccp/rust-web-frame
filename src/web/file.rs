//! File download responses.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A file to be sent to the client as an attachment.
///
/// ```no_run
/// # use rust_web_frame::web::{FileResponse, Response};
/// # fn build() -> Response {
/// Response::file(FileResponse::new("./reports/q3.pdf"))
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileResponse {
    /// Path on disk.
    pub path: PathBuf,
    /// Name offered to the browser. Defaults to the file name of `path`.
    pub file_name: String,
    /// Suffix appended to `file_name` when it is missing, e.g. `.xlsx`.
    pub suffix: String,
}

impl FileResponse {
    /// A download preserving the on-disk file name.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            path,
            file_name,
            suffix: String::new(),
        }
    }

    /// Overrides the name offered to the browser.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.file_name = name.into();
        self
    }

    /// Appends `suffix` when the file name does not already end with it.
    pub fn with_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.suffix = suffix.into();
        self
    }

    /// The file name after the suffix rule has been applied.
    pub fn resolved_file_name(&self) -> String {
        let mut name = if self.file_name.is_empty() {
            self.path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            self.file_name.clone()
        };
        if !self.suffix.is_empty() {
            let suffix = if self.suffix.starts_with('.') {
                self.suffix.clone()
            } else {
                format!(".{}", self.suffix)
            };
            if !name.ends_with(&suffix) {
                name.push_str(&suffix);
            }
        }
        name
    }

    /// The `Content-Disposition` value for this file.
    pub fn content_disposition(&self) -> String {
        format!("attachment; filename=\"{}\"", self.resolved_file_name())
    }

    /// The path on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_defaults_to_the_path() {
        let file = FileResponse::new("./reports/q3.pdf");
        assert_eq!(file.file_name, "q3.pdf");
        assert_eq!(file.resolved_file_name(), "q3.pdf");
    }

    #[test]
    fn suffix_is_appended_once_and_dots_are_optional() {
        let file = FileResponse::new("/tmp/export")
            .with_name("report")
            .with_suffix("xlsx");
        assert_eq!(file.resolved_file_name(), "report.xlsx");

        let again = file.clone().with_suffix(".xlsx");
        assert_eq!(again.resolved_file_name(), "report.xlsx");
    }

    #[test]
    fn content_disposition_is_an_attachment() {
        let file = FileResponse::new("/tmp/a.csv");
        assert_eq!(file.content_disposition(), "attachment; filename=\"a.csv\"");
    }
}

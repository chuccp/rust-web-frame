//! Pagination.

use serde::{Deserialize, Serialize};

/// Page number used when a request does not specify one.
pub const DEFAULT_PAGE_NO: i64 = 1;
/// Page size used when a request does not specify one.
pub const DEFAULT_PAGE_SIZE: i64 = 10;

/// Pagination parameters, as read from a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    /// 1-based page number.
    pub page_no: i64,
    /// Items per page.
    pub page_size: i64,
    /// Cursor for keyset pagination.
    pub last_id: i64,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            page_no: DEFAULT_PAGE_NO,
            page_size: DEFAULT_PAGE_SIZE,
            last_id: 0,
        }
    }
}

impl Page {
    /// A page, clamped to sane bounds.
    pub fn new(page_no: i64, page_size: i64) -> Self {
        Self {
            page_no,
            page_size,
            last_id: 0,
        }
    }

    /// Replaces any value below 1 with the default.
    pub fn with_defaults(self) -> Self {
        Self {
            page_no: if self.page_no < 1 {
                DEFAULT_PAGE_NO
            } else {
                self.page_no
            },
            page_size: if self.page_size < 1 {
                DEFAULT_PAGE_SIZE
            } else {
                self.page_size
            },
            last_id: self.last_id,
        }
    }

    /// The `LIMIT` to send to the database.
    pub fn limit(&self) -> i64 {
        self.with_defaults().page_size
    }

    /// The `OFFSET` to send to the database.
    pub fn offset(&self) -> i64 {
        let page = self.with_defaults();
        (page.page_no - 1) * page.page_size
    }
}

/// A page of results plus the total row count.
///
/// ```no_run
/// # use rust_web_frame::util::{Page, Paged};
/// let page: Paged<String> = Paged::new(42, vec!["a".into(), "b".into()]);
/// assert_eq!(page.total, 42);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paged<T> {
    /// Total number of rows matching the query, ignoring pagination.
    pub total: i64,
    /// The rows on this page.
    pub list: Vec<T>,
}

impl<T> Paged<T> {
    /// Wraps a page of rows.
    pub fn new(total: i64, list: Vec<T>) -> Self {
        Self { total, list }
    }

    /// An empty page.
    pub fn empty() -> Self {
        Self {
            total: 0,
            list: Vec::new(),
        }
    }

    /// Maps the rows, preserving the total.
    pub fn map<U, F: FnMut(T) -> U>(self, f: F) -> Paged<U> {
        Paged {
            total: self.total,
            list: self.list.into_iter().map(f).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_to_missing_values() {
        let page = Page {
            page_no: 0,
            page_size: -3,
            last_id: 0,
        }
        .with_defaults();
        assert_eq!(page.page_no, 1);
        assert_eq!(page.page_size, 10);
    }

    #[test]
    fn offsets_are_zero_based() {
        assert_eq!(Page::new(1, 20).offset(), 0);
        assert_eq!(Page::new(3, 20).offset(), 40);
        assert_eq!(Page::new(3, 20).limit(), 20);
    }

    #[test]
    fn page_able_serialises_under_the_expected_keys() {
        let json = serde_json::to_value(Paged::new(2, vec![1, 2])).unwrap();
        assert_eq!(json, serde_json::json!({"total": 2, "list": [1, 2]}));
    }

    #[test]
    fn page_deserialises_camel_case_request_fields() {
        let page: Page = serde_json::from_value(serde_json::json!({
            "pageNo": 4, "pageSize": 25, "lastId": 9
        }))
        .unwrap();
        assert_eq!(
            page,
            Page {
                page_no: 4,
                page_size: 25,
                last_id: 9
            }
        );
    }
}

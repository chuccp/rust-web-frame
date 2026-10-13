//! Small helpers shared across the framework.

pub mod page;
pub mod str_util;

pub use page::{DEFAULT_PAGE_NO, DEFAULT_PAGE_SIZE, Page, Paged};
pub use str_util::{is_blank, is_not_blank, now_millis, url_decode, url_encode};

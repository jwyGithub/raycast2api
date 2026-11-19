use serde::Serialize;

use crate::error::GatewayError;

/// 管理接口统一的分页入参校验:缺省第 1 页、每页 10 条,`page_size` 限 1..=100。
pub fn page_params(page: Option<u32>, page_size: Option<u32>) -> Result<(u32, u32), GatewayError> {
    let (page, size) = (page.unwrap_or(1), page_size.unwrap_or(10));
    if page == 0 || !(1..=100).contains(&size) {
        return Err(GatewayError::BadRequest(
            "page must be positive and page_size must be in 1..=100".into(),
        ));
    }
    Ok((page, size))
}

#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u32,
    pub page_size: u32,
    pub total_pages: u32,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: u64, page: u32, page_size: u32) -> Self {
        Self {
            items,
            total,
            page,
            page_size,
            total_pages: total.div_ceil(u64::from(page_size)) as u32,
        }
    }
}

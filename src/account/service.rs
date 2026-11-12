//! Account recovery application service; HTTP rendering stays in server handlers.
use crate::{
    account::store::{AccountRow, AccountStore},
    error::GatewayError,
    raycast::client::RaycastClient,
};
use std::sync::Arc;
pub async fn restore(
    store: &Arc<AccountStore>,
    client: &RaycastClient,
    email: &str,
) -> Result<(AccountRow, Option<String>), GatewayError> {
    if !store.reinstate(email)? {
        return Err(GatewayError::NotFound(format!(
            "no account with email {}",
            email
        )));
    }

    // 实时验证:能查通则顺手更新额度信息;查不通不影响"已恢复"这个事实。
    let mut upstream_note: Option<String> = None;
    if let Some(row) = store.get(email)? {
        match crate::account::quota_refresh::fetch_info(client, &row.access_token).await {
            Ok(Some(info)) => {
                store.update_info(email, &info)?;
            }
            Ok(None) => {
                // token 已死:恢复没意义,摘回去并明确告知。
                store.mark_unavailable(email, "token 失效 (401)");
                upstream_note = Some("token 已失效 (401),恢复后仍不可用;需重新授权".into());
            }
            Err(err) if crate::raycast::client::is_account_dead_upstream(&err) => {
                let err = crate::account::quota_refresh::record_refresh_error(store, email, err);
                upstream_note = Some(err.to_string());
            }
            Err(_) => {
                upstream_note =
                    Some("上游查询失败(网络/其他),账号已恢复可用但额度信息未更新".into());
            }
        }
    }

    let row = store
        .get(email)?
        .ok_or_else(|| GatewayError::NotFound(format!("no account with email {}", email)))?;
    Ok((row, upstream_note))
}

//! 批量导入：把 CPA 转换页 / 用户直接粘贴的账号 JSON 规整成 sub2api 批量创建请求体，
//! 并把选中的分组 id 注入每个账号的 `group_ids`。

use anyhow::Result;
use serde_json::{json, Value};

/// 把 CPA 转换页输出（或用户直接粘贴的 sub2api 导入格式）规整成
/// `POST /api/v1/admin/accounts/batch` 的请求体 `{"accounts":[...]}`。
///
/// 兼容三种输入：
/// - `{"accounts":[...]}`（外层已带 accounts）
/// - 裸数组 `[...]`
/// - 单个账号对象 `{...}`
///
/// 每个账号对象都会被写入/覆盖 `group_ids`（= 选中的分组 id 列表，支持多选）。
/// sub2api 的 `group_ids` 是数组，所以「多选分组」= 把这些 id 全部写进每个账号。
pub fn normalize_batch_body(output: &str, group_ids: &[i64]) -> Result<Value> {
    if output.trim().is_empty() {
        anyhow::bail!("转换结果为空，无法导入");
    }
    let v: Value = serde_json::from_str(output)
        .map_err(|e| anyhow::anyhow!("转换结果不是合法 JSON：{}", e))?;
    let accounts = match v {
        Value::Array(a) => a,
        Value::Object(o) => {
            if let Some(acc) = o.get("accounts") {
                acc.as_array()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("accounts 字段不是数组"))?
            } else {
                vec![Value::Object(o)]
            }
        }
        _ => anyhow::bail!("转换结果不是账号数组或对象"),
    };
    let accounts: Vec<Value> = accounts
        .into_iter()
        .filter(|a| !a.is_null())
        .map(|mut a| {
            if let Value::Object(ref mut m) = a {
                m.insert("group_ids".to_string(), json!(group_ids));
            }
            a
        })
        .collect();
    if accounts.is_empty() {
        anyhow::bail!("没有可导入的账号（转换结果为空数组）");
    }
    Ok(json!({ "accounts": accounts }))
}

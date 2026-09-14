//! 账号分组：按 id 升序排序 → 按指定页大小分页 → 每页统一设为一个优先级（= 页码）。
//!
//! sub2api 的调度优先级是「越小越优先」，所以把第 1 页设成 1、第 2 页设成 2 …
//! 等价于把账号按 id 切成若干组，同组账号优先级相同、组与组之间有先后。

use anyhow::{bail, Result};
use serde::Serialize;

use crate::client::Sub2ApiClient;

/// 一页分组的设置结果。
#[derive(Debug, Clone, Serialize)]
pub struct GroupOutcome {
    /// 页码，从 1 开始
    pub page: i64,
    /// 该页账号被设成的优先级（= 页码）
    pub priority: i64,
    /// 该页包含的账号 id（已按 id 升序）
    pub account_ids: Vec<i64>,
    pub ok: bool,
    pub message: String,
}

/// 分组整体结果。
#[derive(Debug, Clone, Serialize)]
pub struct GroupResult {
    /// 每页大小（本次使用的分组粒度，1-20）
    pub page_size: usize,
    /// 参与分组的账号总数
    pub total: usize,
    /// 分出的页数
    pub pages: usize,
    pub outcomes: Vec<GroupOutcome>,
}

/// 允许的分组粒度范围（界面下拉框给出的就是 1-20）。
pub const MIN_PAGE_SIZE: usize = 1;
pub const MAX_PAGE_SIZE: usize = 20;

/// 执行分组：列出全部账号 → 按 id 升序 → 按 `page_size` 分页 → 逐页设优先级 = 页码。
///
/// `page_size` 必须在 1..=20，否则直接报错（调用方应先校验）。
pub fn group_accounts(client: &mut Sub2ApiClient, page_size: usize) -> Result<GroupResult> {
    if page_size < MIN_PAGE_SIZE || page_size > MAX_PAGE_SIZE {
        bail!("每页大小必须在 {} 到 {} 之间", MIN_PAGE_SIZE, MAX_PAGE_SIZE);
    }

    let mut accounts = client.list_accounts()?;
    if accounts.is_empty() {
        return Ok(GroupResult {
            page_size,
            total: 0,
            pages: 0,
            outcomes: Vec::new(),
        });
    }

    // 关键：按 id 从小到大排序，保证分组是稳定可复现的。
    accounts.sort_by_key(|a| a.id);

    let ids: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    let chunks: Vec<&[i64]> = ids.chunks(page_size).collect();

    let mut outcomes = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let page = (i + 1) as i64;
        let outcome = match client.set_priority_bulk(chunk, page) {
            Ok(v) => {
                // bulk-update 返回 { success, failed, results? }；failed>0 也算部分失败。
                let failed = v.get("failed").and_then(|x| x.as_i64()).unwrap_or(0);
                let ok = failed == 0;
                GroupOutcome {
                    page,
                    priority: page,
                    account_ids: chunk.to_vec(),
                    ok,
                    message: if ok {
                        format!("第 {} 页（{} 个账号）优先级已设为 {}", page, chunk.len(), page)
                    } else {
                        format!(
                            "第 {} 页有 {} 个账号设置失败：{}",
                            page,
                            failed,
                            v.get("results")
                                .map(|r| r.to_string())
                                .unwrap_or_else(|| "无明细".to_string())
                        )
                    },
                }
            }
            Err(e) => {
                // 批量端点不可用（老版本 sub2api 没有 bulk-update）→ 回退逐账号 PUT。
                let mut failed = 0usize;
                let mut details: Vec<String> = Vec::new();
                for &id in chunk.iter() {
                    if let Err(e2) = client.set_priority_one(id, page) {
                        failed += 1;
                        details.push(format!("#{}: {}", id, e2));
                    }
                }
                GroupOutcome {
                    page,
                    priority: page,
                    account_ids: chunk.to_vec(),
                    ok: failed == 0,
                    message: if failed == 0 {
                        format!(
                            "第 {} 页（{} 个账号）优先级已设为 {}（批量端点不可用，已逐个写回）",
                            page,
                            chunk.len(),
                            page
                        )
                    } else {
                        format!(
                            "第 {} 页有 {} 个账号设置失败：{}（批量原因：{:#}）",
                            page,
                            failed,
                            details.join("；"),
                            e
                        )
                    },
                }
            }
        };
        outcomes.push(outcome);
    }

    Ok(GroupResult {
        page_size,
        total: ids.len(),
        pages: chunks.len(),
        outcomes,
    })
}

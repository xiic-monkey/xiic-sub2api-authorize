use crate::client::Sub2ApiClient;
use crate::models::Account;

/// 列出账号。
///
/// - 不带 `--401`：打印全部账号 + 末尾汇总（active / 有 error / 含 401 计数）。全部数据按 total 翻页拉全。
/// - 带 `--401`：只打印 error_message 含 "401" 的账号。
/// - 带 `--emails`：只逐行打印账号邮箱（name 字段），便于复制/对接外部流程。
pub fn list(client: &mut Sub2ApiClient, only_401: bool, emails_only: bool) -> anyhow::Result<()> {
    let accounts = client.list_accounts()?;
    let total = accounts.len();

    if total == 0 {
        println!("（没有账号）");
        return Ok(());
    }

    let view: Vec<&Account> = if only_401 {
        accounts.iter().filter(|a| a.has_401()).collect()
    } else {
        accounts.iter().collect()
    };

    if emails_only {
        for a in &view {
            println!("{}", a.name);
        }
        return Ok(());
    }

    let n_401 = accounts.iter().filter(|a| a.has_401()).count();

    if only_401 {
        println!("共 {} 个账号，其中 {} 个报 401：\n", total, n_401);
    } else {
        println!("共 {} 个账号（已按 total 翻页拉全）：\n", total);
    }

    for a in &view {
        let exp = a
            .expires_in_days()
            .map(|d| {
                if d >= 0 {
                    format!("in {}d", d)
                } else {
                    format!("{}d ago", -d)
                }
            })
            .unwrap_or_else(|| "-".to_string());
        println!(
            "#{:<5} {:<22} plat={:<10} type={:<8} status={:<9} sch={} exp={:<8} err={}",
            a.id,
            ellipsize(&a.name, 22),
            a.platform,
            a.account_type,
            a.status,
            if a.schedulable { "Y" } else { "N" },
            exp,
            a.error_message
        );
    }

    if !only_401 {
        let n_err = accounts.iter().filter(|a| a.has_error()).count();
        let n_act = accounts.iter().filter(|a| a.status == "active").count();
        println!(
            "\n--- 汇总: 总 {} | active {} | 有 error {} | 含 401 的 {} ---",
            total, n_act, n_err, n_401
        );
        if n_401 > 0 {
            println!("提示: 用 `accounts list --401` 只看报 401 的账号。");
        }
    }
    Ok(())
}

fn ellipsize(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n - 1).collect();
        format!("{}…", t)
    }
}

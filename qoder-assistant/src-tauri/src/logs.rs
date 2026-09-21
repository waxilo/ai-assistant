use crate::accounts::{set_private_permissions, Account, CheckinRecord};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// 一条签到日志（每次签到尝试都落库，便于追溯）。
///
/// 字段与 [`CheckinRecord`] 一一对应 —— 它只是「加上账号标识、并补一个 id」的落盘形态。
/// 旧版的 `code` / `host` 已随签到线重写一起删掉（见 [`CheckinRecord`] 的说明）。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CheckinLog {
    pub id: String,
    pub account_id: String,
    pub account_name: String,
    /// 账号手机号（展示用，可空）
    #[serde(default)]
    pub account_phone: Option<String>,
    /// 账号邮箱（展示用，可空）。与手机号一样是「这个人的标识」——
    /// 国内版看手机号、国际版看邮箱，日志页与账号页因此长得一样。
    #[serde(default)]
    pub account_email: Option<String>,
    pub at: String,
    pub success: bool,
    pub already: bool,
    pub inactive: bool,
    pub message: String,
    /// 本次领取到的额度
    #[serde(default)]
    pub credit: Option<f64>,
    /// 剩余积分（小数）
    #[serde(default)]
    pub balance: Option<f64>,
    /// 领的是哪一场活动（`campaignKey`）
    #[serde(default)]
    pub campaign_key: Option<String>,
    /// 这笔积分的到期时刻（毫秒）；见 [`CheckinRecord::expires_at`]。
    /// 老日志没有这个键（默认 `None`），读取侧按「未知」展示。
    #[serde(default)]
    pub expires_at: Option<i64>,
}

/// 最多保留的日志条数，超出后丢弃最旧的，避免无限膨胀
const MAX_LOGS: usize = 2000;

pub fn logs_file(dir: &Path) -> PathBuf {
    dir.join("checkin_logs.json")
}

pub fn load_logs(dir: &Path) -> Vec<CheckinLog> {
    let f = logs_file(dir);
    if !f.exists() {
        return Vec::new();
    }
    let s = fs::read_to_string(&f).unwrap_or_default();
    serde_json::from_str(&s).unwrap_or_default()
}

fn save_logs(dir: &Path, logs: &[CheckinLog]) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let target = logs_file(dir);
    let tmp = dir.join("checkin_logs.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(logs)?)?;
    fs::rename(&tmp, &target)?;
    set_private_permissions(&target);
    Ok(())
}

/// 追加一条日志；保留最近 MAX_LOGS 条
pub fn append_log(dir: &Path, log: CheckinLog) -> std::io::Result<()> {
    let mut logs = load_logs(dir);
    logs.push(log);
    if logs.len() > MAX_LOGS {
        let drop = logs.len() - MAX_LOGS;
        logs.drain(0..drop);
    }
    save_logs(dir, &logs)
}

/// 清空日志：`account_id` 为 None 时清空全部，否则只清该账号的日志
pub fn clear_logs(dir: &Path, account_id: Option<&str>) -> std::io::Result<()> {
    match account_id {
        None => save_logs(dir, &[]),
        Some(id) => {
            let mut logs = load_logs(dir);
            logs.retain(|l| l.account_id != id);
            save_logs(dir, &logs)
        }
    }
}

/// 由账号 + 签到结果构造一条日志（复制需要的字段，不移动 rec）
pub fn log_from_record(account: &Account, rec: &CheckinRecord) -> CheckinLog {
    CheckinLog {
        id: uuid::Uuid::new_v4().to_string(),
        account_id: account.id.clone(),
        account_name: account.name.clone(),
        account_phone: account.phone.clone(),
        account_email: account.email.clone(),
        at: rec.at.clone(),
        success: rec.success,
        already: rec.already,
        inactive: rec.inactive,
        message: rec.message.clone(),
        credit: rec.credit,
        balance: rec.balance,
        campaign_key: rec.campaign_key.clone(),
        expires_at: rec.expires_at,
    }
}

//! sub2api 外部运维核心库（不修改上游，纯 admin API 驱动）。
//!
//! 同一个 crate 服务两个消费者：
//! - `sub2api-operator`（bin）：命令行 / 服务器（Docker）运维；
//! - `xiic-sub2api-auth-desktop`（Tauri）：本地单机 GUI，path 依赖本库复用同一套逻辑。
//!
//! 模块职责：
//! - [`client`]  admin API 客户端（登录 / 2FA / 刷新 / 分页拉全 / 写回凭证）
//! - [`models`]  账号 DTO
//! - [`config`]  config.toml 读写
//! - [`store`]   本地凭证库（账号 / 密码 / CDK）
//! - [`browser`] 原生浏览器自动化（chromiumoxide 直连系统 Chrome/Chromium 的 CDP，零 Node）
//! - [`commands`] CLI 侧的流程编排（账号列表、重授权写回）
//! - [`totp`]    2FA 验证码
//! - [`web`]     本地凭证录入页（axum）

pub mod browser;
pub mod client;
pub mod commands;
pub mod config;
pub mod models;
pub mod store;
pub mod totp;
pub mod web;

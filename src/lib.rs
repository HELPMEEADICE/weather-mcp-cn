//! `weather-mcp-cn` —— 基于中国天气网公开数据的 MCP 天气服务器。
//!
//! 设计约束：
//!
//! * 只用公开网页与公开数据接口，不需要 API Key、不依赖 Python/Node/浏览器运行时；
//! * 所有气象数值都来自真实抓取，绝不使用模拟数据；缺失字段留空而不是填默认值；
//! * 严格区分「观测时间」「预报发布时间」「本地抓取时间」；
//! * 单个工具调用失败只返回 MCP 错误，不会让服务器进程崩溃。
//!
//! 模块划分：
//!
//! * [`config`]：命令行/环境变量配置；
//! * [`error`]：错误类型与 MCP 错误映射；
//! * [`http`]：带限速、重试、编码处理的 HTTP 层；
//! * [`cache`]：TTL 内存缓存；
//! * [`city`]：中文地名 → 中国天气网城市代码；
//! * [`citygen`]：城市库抓取生成器；
//! * [`models`]：统一的天气数据模型；
//! * [`providers`]：数据源 trait 与 `weather.com.cn` 实现；
//! * [`tools`] / [`server`]：MCP 工具与服务器。

pub mod cache;
pub mod city;
pub mod citygen;
pub mod config;
pub mod error;
pub mod http;
pub mod models;
pub mod providers;
pub mod server;
pub mod tools;

/// 内置城市库的原始 JSON（编译期嵌入，发布物不依赖外部文件）。
pub const EMBEDDED_CITIES_JSON: &str = include_str!("../data/cities.json");

/// 版本号。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

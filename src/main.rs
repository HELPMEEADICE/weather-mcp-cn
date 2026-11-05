//! `weather-mcp-cn` 可执行入口。
//!
//! 注意：stdio 传输下 **stdout 只能输出 MCP 协议消息**，因此日志一律写到 stderr，
//! 只有 `--help` / `--version` 这类一次性命令行输出才会写 stdout。

use std::process::ExitCode;
use std::sync::Arc;

use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use weather_mcp_cn::city::CityIndex;
use weather_mcp_cn::citygen;
use weather_mcp_cn::config::{Command, Config, HELP};
use weather_mcp_cn::http::HttpClient;
use weather_mcp_cn::providers::weather_com_cn::WeatherComCn;
use weather_mcp_cn::server::WeatherServer;

fn main() -> ExitCode {
    init_tracing();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("无法创建异步运行时：{err}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("weather-mcp-cn 启动失败：{err}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Config::from_env_and_args()? {
        Command::Help => {
            println!("{HELP}");
            Ok(())
        }
        Command::Version => {
            println!("weather-mcp-cn {}", weather_mcp_cn::VERSION);
            Ok(())
        }
        Command::RefreshCityIndex { config, output } => {
            let http = HttpClient::new(&config)?;
            let file = citygen::write_city_index(&http, &output).await?;
            eprintln!("城市库已生成：{} 条 → {}", file.total, output.display());
            Ok(())
        }
        Command::Run(config) => serve(Arc::new(*config)).await,
    }
}

async fn serve(config: Arc<Config>) -> Result<(), Box<dyn std::error::Error>> {
    let external_index = std::env::var("WEATHER_MCP_CITIES_FILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from);

    let index = match external_index.as_deref() {
        Some(path) => {
            tracing::info!(path = %path.display(), "使用外部城市库");
            CityIndex::from_path(path)?
        }
        None => CityIndex::from_json(weather_mcp_cn::EMBEDDED_CITIES_JSON)?,
    };
    tracing::info!(
        cities = index.len(),
        generated_at = index.generated_at(),
        "城市库已加载"
    );

    let provider = Arc::new(WeatherComCn::new(&config)?);
    let server = WeatherServer::new(provider, Arc::new(index));

    tracing::info!(
        transport = config.transport.as_str(),
        version = weather_mcp_cn::VERSION,
        "MCP 服务器已启动（stdio）"
    );

    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    tracing::info!("MCP 服务器已退出");
    Ok(())
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .try_init();
}

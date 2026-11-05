//! MCP 协议级测试：真正启动编译好的 `weather-mcp-cn` 可执行文件，
//! 通过 stdio 完成 `initialize` → `notifications/initialized` → `tools/list` → `tools/call`
//! 全流程。这里不调用任何 Rust 函数，验证的是线上协议行为。
//!
//! 这几个用例完全离线：只调用不访问上游的 `search_cities`，以及用错误参数触发
//! 本地校验失败，因此 `cargo test` 不依赖网络。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(60);

/// 一个极简的 MCP stdio 客户端：按行发送 JSON-RPC，按行读取响应。
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
}

impl McpClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_weather-mcp-cn"))
            .arg("--transport")
            .arg("stdio")
            .env("RUST_LOG", "warn")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("应能启动 weather-mcp-cn");

        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = channel();

        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(trimmed).unwrap_or_else(|err| {
                    panic!("stdout 出现了非 JSON 内容（协议流被污染）：{trimmed} / {err}")
                });
                assert_eq!(
                    value.get("jsonrpc").and_then(Value::as_str),
                    Some("2.0"),
                    "stdout 每一行都必须是 JSON-RPC 2.0 消息"
                );
                if tx.send(value).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            rx,
            next_id: 0,
        }
    }

    fn write_message(&mut self, message: &Value) {
        let mut line = serde_json::to_string(message).expect("序列化请求");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .expect("写入 stdin 失败");
        self.stdin.flush().expect("flush stdin 失败");
    }

    fn notify(&mut self, method: &str) {
        self.write_message(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// 发送请求并等待同 id 的响应（自动跳过服务端通知）。
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }));

        loop {
            match self.rx.recv_timeout(TIMEOUT) {
                Ok(value) => {
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        return value;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("等待 {method} 响应超时（{TIMEOUT:?}）")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("服务端在响应 {method} 之前关闭了 stdout")
                }
            }
        }
    }

    fn initialize(&mut self) -> Value {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "weather-mcp-cn-tests", "version": "0.0.0"},
            }),
        );
        let result = response.get("result").cloned().unwrap_or_else(|| {
            panic!("initialize 应返回 result，实际：{response}");
        });
        self.notify("notifications/initialized");
        result
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool_names(list_result: &Value) -> Vec<String> {
    list_result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools/list 必须返回 tools 数组")
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .expect("每个工具都要有 name")
                .to_string()
        })
        .collect()
}

/// 从 `tools/call` 的 result 里取出结构化内容。
fn structured(result: &Value) -> Value {
    assert_ne!(
        result.get("isError").and_then(Value::as_bool),
        Some(true),
        "工具调用返回了错误：{result}"
    );
    if let Some(value) = result.get("structuredContent") {
        return value.clone();
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .expect("CallToolResult 必须带 content 或 structuredContent");
    serde_json::from_str(text).expect("content[0].text 应当是 JSON")
}

#[test]
fn initializes_and_lists_tools() {
    let mut client = McpClient::spawn();
    let init = client.initialize();

    assert_eq!(
        init.pointer("/serverInfo/name").and_then(Value::as_str),
        Some("weather-mcp-cn"),
        "serverInfo.name 不对：{init}"
    );
    assert_eq!(
        init.pointer("/serverInfo/version").and_then(Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert!(
        init.pointer("/capabilities/tools").is_some(),
        "必须声明 tools 能力：{init}"
    );
    let instructions = init
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or_default();
    assert!(
        instructions.contains("3 小时"),
        "instructions 应说明逐时数据的粒度"
    );

    let listed = client.request("tools/list", json!({}));
    let result = listed.get("result").expect("tools/list 应有 result");
    let names = tool_names(result);
    for expected in [
        "get_current_weather",
        "get_weather_forecast",
        "get_hourly_weather",
        "search_cities",
        "get_weather_alerts",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "缺少工具 {expected}，实际 {names:?}"
        );
    }

    // 每个工具都必须有非空 description 与合法的 inputSchema。
    for tool in result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools 数组")
    {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(description.len() > 20, "工具 {name} 的描述太短");
        let schema = tool.get("inputSchema").expect("每个工具都要有 inputSchema");
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "工具 {name} 的 inputSchema 必须是 object"
        );
    }

    // 参数 schema 具体内容抽查。
    let forecast = result
        .get("tools")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == json!("get_weather_forecast"))
        .expect("应有 get_weather_forecast");
    let properties = forecast
        .pointer("/inputSchema/properties")
        .expect("应有 properties");
    assert!(properties.get("city").is_some(), "应声明 city 参数");
    assert!(properties.get("days").is_some(), "应声明 days 参数");
    let required = forecast
        .pointer("/inputSchema/required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert_eq!(required, vec![json!("city")], "只有 city 是必填");
}

#[test]
fn search_cities_returns_structured_city_data() {
    let mut client = McpClient::spawn();
    client.initialize();

    let response = client.call_tool("search_cities", json!({"query": "南山", "limit": 5}));
    let result = response.get("result").unwrap_or_else(|| {
        panic!("search_cities 应成功，实际：{response}");
    });
    let payload = structured(result);

    assert_eq!(payload.get("query").and_then(Value::as_str), Some("南山"));
    let cities = payload
        .get("cities")
        .and_then(Value::as_array)
        .expect("应返回 cities 数组");
    assert!(!cities.is_empty(), "应命中南山相关城市");
    let codes: Vec<&str> = cities
        .iter()
        .filter_map(|city| city.get("code").and_then(Value::as_str))
        .collect();
    assert!(
        codes.contains(&"101280604"),
        "应包含深圳南山 101280604，实际 {codes:?}"
    );
    let first = &cities[0];
    assert!(first.get("display_name").is_some());
    assert!(first.get("province").is_some());
    assert!(payload.pointer("/city_index/total").is_some());
    let total = payload
        .pointer("/city_index/total")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert!(total > 3000, "内置城市库条目数异常：{total}");
}

#[test]
fn unknown_tool_is_rejected_without_crashing() {
    let mut client = McpClient::spawn();
    client.initialize();

    let response = client.call_tool("no_such_tool", json!({}));
    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("未知工具应返回 JSON-RPC 错误，实际：{response}"));
    assert!(error.get("code").and_then(Value::as_i64).is_some());

    // 出错之后服务器仍然可用。
    let response = client.call_tool("search_cities", json!({"query": "天河"}));
    assert!(response.get("result").is_some(), "错误不应让服务器崩溃");
}

#[test]
fn invalid_city_returns_structured_error() {
    let mut client = McpClient::spawn();
    client.initialize();

    for args in [
        json!({"city": "不存在的城市名称"}),
        json!({"city": ""}),
        json!({"city": "鼓楼"}),
    ] {
        let response = client.call_tool("get_current_weather", args);
        let error = response.get("error").unwrap_or_else(|| {
            panic!("非法城市应返回错误而不是伪造结果，实际：{response}");
        });
        let data = error.get("data").cloned().unwrap_or(Value::Null);
        let code = data.get("code").and_then(Value::as_str).unwrap_or("");
        assert!(
            code == "city_not_found" || code == "ambiguous_city",
            "错误码不对：{code} / {data}"
        );
    }
}

#[test]
fn missing_required_argument_is_rejected() {
    let mut client = McpClient::spawn();
    client.initialize();

    let response = client.call_tool("get_weather_forecast", json!({}));
    // MCP 允许两种报错方式：JSON-RPC error，或带 isError=true 的工具结果。
    let is_tool_error = response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        response.get("error").is_some() || is_tool_error,
        "缺少 city 参数应当报错，实际：{response}"
    );
    if is_tool_error {
        let text = response
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(text.contains("city"), "错误信息应指出缺少的参数：{text}");
    }
}

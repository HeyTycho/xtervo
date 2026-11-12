// SPDX-License-Identifier: GPL-3.0-only
//! JSON-RPC 2.0 line-delimited 协议类型、宿主/插件方法名与声明式 UI 校验。
//!
//! 一条消息 = 一行 UTF-8 JSON（以 `\n` 结束）。宿主和插件都必须忽略空行；
//! 坏 JSON、坏结构、超长行由 supervisor 计数并最终重启/禁用插件。
//!
//! 这里不依赖 Slint：声明式面板/settings 先解析成固定元素类型，UI 层再映射到
//! 宿主自己的组件。插件无法注入任意 Slint/WASM/HTML。

use serde_json::{json, Value};

use super::manifest::{validate_id, PluginEvent};

pub const JSONRPC_VERSION: &str = "2.0";
pub const HOST_PROTOCOL_VERSION: u32 = 3;

/// 协议单行上限（字节）。超过后只保留截断行并计坏行，避免无界分配。
pub const MAX_LINE_BYTES: usize = 256 * 1024;
/// 每个插件进程 stdout 总输出上限（字节）；超过即按协议错误重启。
pub const MAX_OUTPUT_BYTES: u64 = 16 * 1024 * 1024;
/// stderr 保留的尾部字节数（只用于错误展示，不写入设置页正文）。
pub const MAX_STDERR_TAIL_BYTES: usize = 8 * 1024;

pub const MAX_SURFACE_ELEMENTS: usize = 100;
pub const MAX_SURFACE_TITLE_CHARS: usize = 120;
pub const MAX_SURFACE_TEXT_CHARS: usize = 4000;
pub const MAX_LIST_ITEMS: usize = 50;
pub const MAX_LIST_ITEM_CHARS: usize = 500;
pub const MAX_ELEMENT_ID_CHARS: usize = 64;
pub const MAX_INPUT_VALUE_CHARS: usize = 1000;
pub const MAX_SELECT_OPTIONS: usize = 32;
pub const MAX_NOTIFY_CHARS: usize = 500;
pub const MAX_STATUS_CHARS: usize = 200;
pub const MAX_TOOLTIP_CHARS: usize = 512;
pub const MAX_TERMINAL_READ_LINES: u32 = 500;
pub const MAX_TERMINAL_READ_BYTES: u32 = 64 * 1024;
pub const MAX_TERMINAL_WRITE_BYTES: usize = 4096;
pub const MAX_CLIPBOARD_CHARS: usize = 64 * 1024;
pub const MAX_COMMAND_ARGS: usize = 32;
pub const MAX_COMMAND_ARG_CHARS: usize = 256;
pub const MAX_RESULT_TEXT_CHARS: usize = 2000;

// ---------------------------------------------------------------- 方法名
//
// 宿主 -> 插件。
pub const M_INITIALIZE: &str = "initialize";
pub const M_INITIALIZED: &str = "initialized";
pub const M_COMMAND_INVOKE: &str = "command.invoke";
pub const M_EVENT_DELIVER: &str = "event.deliver";
pub const M_STATUS_GET: &str = "status.get";
pub const M_SHUTDOWN: &str = "shutdown";
pub const M_PING: &str = "ping";
pub const M_SURFACE_RENDER: &str = "surface.render";
pub const M_UI_ACTION: &str = "ui.action";

// 插件 -> 宿主。
pub const M_UI_NOTIFY: &str = "ui.notify";
pub const M_UI_STATUS: &str = "ui.status";
pub const M_UI_RENDER: &str = "ui.render";
pub const M_CONTEXT_VERSION: &str = "context.version";
pub const M_CONTEXT_CONFIG_METADATA: &str = "context.config_metadata";
pub const M_TERMINAL_READ: &str = "terminal.read";
pub const M_TERMINAL_WRITE: &str = "terminal.write";
pub const M_CLIPBOARD_READ: &str = "clipboard.read";
pub const M_CLIPBOARD_WRITE: &str = "clipboard.write";
pub const M_CREDENTIALS_GET: &str = "credentials.get";

/// JSON-RPC / 宿主错误码。
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const PERMISSION_DENIED: i64 = -32001;
    pub const UNSUPPORTED: i64 = -32002;
    pub const DENIED_BY_USER: i64 = -32003;
    pub const UNAVAILABLE: i64 = -32004;
    pub const RATE_LIMITED: i64 = -32005;
    pub const NOT_APPROVED: i64 = -32006;
    pub const REQUEST_TIMEOUT: i64 = -32007;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpcId(pub Value);

impl RpcId {
    pub fn number(id: u64) -> Self {
        Self(Value::from(id))
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.0.as_u64()
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.0.as_i64()
    }

    pub fn to_value(&self) -> Value {
        self.0.clone()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<RpcError>,
    },
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// 结构不合法/不是 JSON-RPC 2.0；supervisor 计入坏行。
    Invalid(String),
}

/// 构造宿主 -> 插件请求。
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "method": method,
        "params": params,
    })
}

/// 构造宿主 -> 插件通知。
pub fn notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": method,
        "params": params,
    })
}

/// 构造宿主 -> 插件成功响应。
pub fn result(id: &RpcId, value: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.to_value(),
        "result": value,
    })
}

/// 构造宿主 -> 插件错误响应。
pub fn error(id: &RpcId, err: &RpcError) -> Value {
    let mut error = json!({
        "code": err.code,
        "message": err.message,
    });
    if let Some(data) = &err.data {
        error["data"] = data.clone();
    }
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.to_value(),
        "error": error,
    })
}

/// 解析一行插件输出。
pub fn parse_incoming(line: &str) -> Incoming {
    let line = line.trim();
    if line.is_empty() {
        return Incoming::Invalid("空行".to_string());
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => return Incoming::Invalid(format!("JSON 解析失败: {e}")),
    };
    let Some(object) = value.as_object() else {
        return Incoming::Invalid("顶层不是 JSON object".to_string());
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
        return Incoming::Invalid("缺少 jsonrpc = \"2.0\"".to_string());
    }
    let has_method = object.contains_key("method");
    let has_id = object.contains_key("id");
    if has_method {
        if object.contains_key("result") || object.contains_key("error") {
            return Incoming::Invalid("request/notification 不能包含 result/error".to_string());
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Incoming::Invalid("method 不是字符串".to_string());
        };
        if method.trim().is_empty() {
            return Incoming::Invalid("method 为空".to_string());
        }
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        if params.is_null() {
            // JSON-RPC 允许省略/null params；协议层统一当空对象。
        } else if !params.is_object() && !params.is_array() {
            return Incoming::Invalid("params 必须是 object/array/缺省".to_string());
        }
        if has_id {
            let Some(id) = object.get("id") else {
                return Incoming::Invalid("缺少 id".to_string());
            };
            if !(id.is_u64() || id.is_i64() || id.is_string()) {
                return Incoming::Invalid("request id 必须是 number/string".to_string());
            }
            Incoming::Request {
                id: RpcId(id.clone()),
                method: method.to_string(),
                params: if params.is_null() { json!({}) } else { params },
            }
        } else {
            Incoming::Notification {
                method: method.to_string(),
                params: if params.is_null() { json!({}) } else { params },
            }
        }
    } else if has_id {
        let Some(id) = object.get("id").and_then(Value::as_u64) else {
            return Incoming::Invalid("response id 必须是宿主分配的 u64".to_string());
        };
        let result = object.get("result").cloned();
        let error = object.get("error").map(|error| {
            RpcError {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(error_code::INTERNAL_ERROR),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_string(),
                data: error.get("data").cloned(),
            }
        });
        if result.is_none() && error.is_none() {
            return Incoming::Invalid("response 缺少 result/error".to_string());
        }
        if result.is_some() && error.is_some() {
            return Incoming::Invalid("response 同时包含 result 与 error".to_string());
        }
        Incoming::Response { id, result, error }
    } else {
        Incoming::Invalid("既不是 request/notification 也不是 response".to_string())
    }
}

/// 命令调用的参数校验：最多 32 个短字符串。
pub fn parse_command_args(value: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(array) = value.as_array() else {
        return Err("args 必须是字符串数组".to_string());
    };
    if array.len() > MAX_COMMAND_ARGS {
        return Err(format!("args 最多 {MAX_COMMAND_ARGS} 个"));
    }
    let mut args = Vec::with_capacity(array.len());
    for item in array {
        let Some(text) = item.as_str() else {
            return Err("args 只能是字符串".to_string());
        };
        if text.chars().count() > MAX_COMMAND_ARG_CHARS {
            return Err(format!("单个 arg 超过 {MAX_COMMAND_ARG_CHARS} 字符"));
        }
        args.push(text.to_string());
    }
    Ok(args)
}

/// 命令结果 -> (ok, 展示文本)。结果可以是字符串、`{ok,message}` 或任意 JSON。
pub fn format_command_result(value: &Value) -> (bool, String) {
    match value {
        Value::Null => (true, "ok".to_string()),
        Value::String(text) => (true, clip_chars(text, MAX_RESULT_TEXT_CHARS)),
        Value::Object(object) => {
            let ok = object.get("ok").and_then(Value::as_bool).unwrap_or(true);
            let text = object
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| object.get("text").and_then(Value::as_str))
                .map(|text| clip_chars(text, MAX_RESULT_TEXT_CHARS))
                .unwrap_or_else(|| {
                    clip_chars(&Value::Object(object.clone()).to_string(), MAX_RESULT_TEXT_CHARS)
                });
            (ok, text)
        },
        other => (true, clip_chars(&other.to_string(), MAX_RESULT_TEXT_CHARS)),
    }
}

/// `status.get` / `ui.status` 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusPayload {
    pub text: String,
    pub tooltip: String,
    pub command: Option<String>,
}

pub fn parse_status_payload(value: &Value) -> Result<StatusPayload, String> {
    let object = value.as_object().ok_or("status 结果必须是 object")?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or("status.text 必须是字符串")?;
    if text.chars().count() > MAX_STATUS_CHARS {
        return Err(format!("status.text 超过 {MAX_STATUS_CHARS} 字符"));
    }
    let tooltip = object
        .get("tooltip")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if tooltip.chars().count() > MAX_TOOLTIP_CHARS {
        return Err(format!("status.tooltip 超过 {MAX_TOOLTIP_CHARS} 字符"));
    }
    let command = object
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(|command| command.to_string());
    if command.as_ref().is_some_and(|command| command.chars().count() > 128) {
        return Err("status.command 超过 128 字符".into());
    }
    Ok(StatusPayload { text: text.to_string(), tooltip, command })
}

/// `ui.notify` 参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyPayload {
    pub text: String,
    pub level: String,
}

pub fn parse_notify_payload(params: &Value) -> Result<NotifyPayload, String> {
    let object = params.as_object().ok_or("ui.notify params 必须是 object")?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or("ui.notify.text 不能为空")?;
    if text.chars().count() > MAX_NOTIFY_CHARS {
        return Err(format!("ui.notify.text 超过 {MAX_NOTIFY_CHARS} 字符"));
    }
    let level = object
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let level = match level {
        "info" | "warn" | "error" => level,
        other => return Err(format!("ui.notify.level 不支持 {other:?}")),
    };
    Ok(NotifyPayload { text: text.to_string(), level: level.to_string() })
}

/// `terminal.read` 参数（被宿主 clamp）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalReadLimits {
    pub max_lines: u32,
    pub max_bytes: u32,
}

pub fn parse_terminal_read_params(params: &Value) -> TerminalReadLimits {
    let defaults = TerminalReadLimits {
        max_lines: 80,
        max_bytes: 32 * 1024,
    };
    let Some(object) = params.as_object() else {
        return defaults;
    };
    let max_lines = object
        .get("max_lines")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, MAX_TERMINAL_READ_LINES as u64) as u32)
        .unwrap_or(defaults.max_lines);
    let max_bytes = object
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, MAX_TERMINAL_READ_BYTES as u64) as u32)
        .unwrap_or(defaults.max_bytes);
    TerminalReadLimits { max_lines, max_bytes }
}

/// `terminal.write` 参数。
pub fn parse_terminal_write_params(params: &Value) -> Result<String, String> {
    let object = params.as_object().ok_or("terminal.write params 必须是 object")?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or("terminal.write.text 必须是字符串")?;
    if text.is_empty() {
        return Err("terminal.write.text 不能为空".into());
    }
    if text.len() > MAX_TERMINAL_WRITE_BYTES {
        return Err(format!(
            "terminal.write.text 超过 {MAX_TERMINAL_WRITE_BYTES} 字节"
        ));
    }
    Ok(text.to_string())
}

/// `clipboard.write` 参数。
pub fn parse_clipboard_write_params(params: &Value) -> Result<String, String> {
    let object = params.as_object().ok_or("clipboard.write params 必须是 object")?;
    let text = object
        .get("text")
        .and_then(Value::as_str)
        .ok_or("clipboard.write.text 必须是字符串")?;
    if text.chars().count() > MAX_CLIPBOARD_CHARS {
        return Err(format!(
            "clipboard.write.text 超过 {MAX_CLIPBOARD_CHARS} 字符"
        ));
    }
    Ok(text.to_string())
}

// ---------------------------------------------------------------- 声明式 UI

/// 宿主允许渲染的固定元素类型；插件不能提供布局/样式/任意控件。
#[derive(Clone, Debug, PartialEq)]
pub enum SurfaceElement {
    Text { text: String },
    Markdown { text: String },
    List { items: Vec<String> },
    Input { id: String, label: String, value: String, placeholder: String },
    Button { id: String, label: String },
    Select { id: String, label: String, value: String, options: Vec<String> },
    Progress { label: String, value: f32 },
}

impl SurfaceElement {
    pub fn kind(&self) -> &'static str {
        match self {
            SurfaceElement::Text { .. } => "text",
            SurfaceElement::Markdown { .. } => "markdown",
            SurfaceElement::List { .. } => "list",
            SurfaceElement::Input { .. } => "input",
            SurfaceElement::Button { .. } => "button",
            SurfaceElement::Select { .. } => "select",
            SurfaceElement::Progress { .. } => "progress",
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            SurfaceElement::Input { id, .. }
            | SurfaceElement::Button { id, .. }
            | SurfaceElement::Select { id, .. } => Some(id),
            _ => None,
        }
    }

    pub fn value(&self) -> Option<&str> {
        match self {
            SurfaceElement::Input { value, .. } | SurfaceElement::Select { value, .. } => {
                Some(value)
            },
            _ => None,
        }
    }
}

/// 一份声明式面板/settings 文档。
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceDoc {
    pub title: String,
    pub elements: Vec<SurfaceElement>,
}

impl SurfaceDoc {
    pub fn element(&self, id: &str) -> Option<&SurfaceElement> {
        self.elements.iter().find(|element| element.id() == Some(id))
    }

    pub fn input_value(&self, id: &str) -> Option<&str> {
        self.elements.iter().find_map(|element| match element {
            SurfaceElement::Input { id: element_id, value, .. } if element_id == id => {
                Some(value.as_str())
            },
            _ => None,
        })
    }

    /// 用户切换 select 时宿主计算下一个候选值，再通过 `ui.action` 发给插件。
    pub fn next_select_value(&self, id: &str) -> Option<String> {
        self.elements.iter().find_map(|element| match element {
            SurfaceElement::Select { id: element_id, value, options, .. }
                if element_id == id =>
            {
                if options.is_empty() {
                    return None;
                }
                let index = options.iter().position(|option| option == value).unwrap_or(0);
                Some(options[(index + 1) % options.len()].clone())
            },
            _ => None,
        })
    }
}

/// 解析声明式 UI 文档；任何未知字段/类型/超限都会拒绝整份文档。
pub fn parse_surface_doc(value: &Value) -> Result<SurfaceDoc, String> {
    let object = value.as_object().ok_or("UI 文档必须是 object")?;
    if let Some(version) = object.get("version") {
        if version.as_u64() != Some(1) {
            return Err("UI 文档 version 只支持 1".into());
        }
    }
    let title = object.get("title").and_then(Value::as_str).unwrap_or("");
    if title.chars().count() > MAX_SURFACE_TITLE_CHARS {
        return Err(format!("UI title 超过 {MAX_SURFACE_TITLE_CHARS} 字符"));
    }
    let elements = object
        .get("elements")
        .and_then(Value::as_array)
        .ok_or("UI 文档缺少 elements 数组")?;
    if elements.len() > MAX_SURFACE_ELEMENTS {
        return Err(format!("UI 元素超过 {MAX_SURFACE_ELEMENTS} 个"));
    }
    let mut parsed = Vec::with_capacity(elements.len());
    let mut ids: Vec<String> = Vec::new();
    for (index, raw) in elements.iter().enumerate() {
        let element = parse_surface_element(raw)
            .map_err(|e| format!("elements[{index}]: {e}"))?;
        if let Some(id) = element.id() {
            if ids.iter().any(|seen| seen == id) {
                return Err(format!("elements[{index}]: 元素 id 重复 {id:?}"));
            }
            ids.push(id.to_string());
        }
        parsed.push(element);
    }
    Ok(SurfaceDoc { title: title.to_string(), elements: parsed })
}

fn parse_surface_element(value: &Value) -> Result<SurfaceElement, String> {
    let object = value.as_object().ok_or("元素必须是 object")?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or("元素缺少 type")?;
    let require_text = |field: &str, max: usize| -> Result<String, String> {
        let text = object
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{kind}.{field} 必须是字符串"))?;
        if text.chars().count() > max {
            return Err(format!("{kind}.{field} 超过 {max} 字符"));
        }
        Ok(text.to_string())
    };
    let element_id = |object: &serde_json::Map<String, Value>| -> Result<String, String> {
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("{kind}.id 不能为空"))?;
        if id.chars().count() > MAX_ELEMENT_ID_CHARS {
            return Err(format!(
                "{kind}.id 超过 {MAX_ELEMENT_ID_CHARS} 字符"
            ));
        }
        validate_id(id, &format!("{kind}.id"))?;
        Ok(id.to_string())
    };
    match kind {
        "text" => Ok(SurfaceElement::Text {
            text: require_text("text", MAX_SURFACE_TEXT_CHARS)?,
        }),
        "markdown" => Ok(SurfaceElement::Markdown {
            text: require_text("text", MAX_SURFACE_TEXT_CHARS)?,
        }),
        "list" => {
            let items = object
                .get("items")
                .and_then(Value::as_array)
                .ok_or("list.items 必须是数组")?;
            if items.len() > MAX_LIST_ITEMS {
                return Err(format!("list.items 最多 {MAX_LIST_ITEMS} 项"));
            }
            let mut parsed = Vec::with_capacity(items.len());
            for item in items {
                let text = item.as_str().ok_or("list.items 只能是字符串")?;
                if text.chars().count() > MAX_LIST_ITEM_CHARS {
                    return Err(format!(
                        "list 单项超过 {MAX_LIST_ITEM_CHARS} 字符"
                    ));
                }
                parsed.push(text.to_string());
            }
            Ok(SurfaceElement::List { items: parsed })
        },
        "input" => {
            let id = element_id(object)?;
            let value = object
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if value.chars().count() > MAX_INPUT_VALUE_CHARS {
                return Err(format!(
                    "input.value 超过 {MAX_INPUT_VALUE_CHARS} 字符"
                ));
            }
            let label = object
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            let placeholder = object
                .get("placeholder")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if placeholder.chars().count() > 120 {
                return Err("input.placeholder 超过 120 字符".into());
            }
            Ok(SurfaceElement::Input { id, label, value, placeholder })
        },
        "button" => {
            let id = element_id(object)?;
            let label = object
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .ok_or("button.label 不能为空")?;
            if label.chars().count() > 120 {
                return Err("button.label 超过 120 字符".into());
            }
            Ok(SurfaceElement::Button { id, label: label.to_string() })
        },
        "select" => {
            let id = element_id(object)?;
            let options = object
                .get("options")
                .and_then(Value::as_array)
                .ok_or("select.options 必须是数组")?;
            if options.is_empty() || options.len() > MAX_SELECT_OPTIONS {
                return Err(format!(
                    "select.options 数量必须在 1..={MAX_SELECT_OPTIONS}"
                ));
            }
            let mut parsed = Vec::with_capacity(options.len());
            for option in options {
                let text = option.as_str().ok_or("select.options 只能是字符串")?;
                if text.chars().count() > 200 {
                    return Err("select 选项超过 200 字符".into());
                }
                parsed.push(text.to_string());
            }
            let value = object
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or(&parsed[0])
                .to_string();
            if !parsed.iter().any(|option| option == &value) {
                return Err(format!("select.value {value:?} 不在 options 内"));
            }
            let label = object
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or(&id)
                .to_string();
            Ok(SurfaceElement::Select { id, label, value, options: parsed })
        },
        "progress" => {
            let value = object
                .get("value")
                .and_then(Value::as_f64)
                .ok_or("progress.value 必须是数字")?;
            if !(0.0..=1.0).contains(&value) {
                return Err("progress.value 必须在 0..=1".into());
            }
            let label = object.get("label").and_then(Value::as_str).unwrap_or("");
            if label.chars().count() > 120 {
                return Err("progress.label 超过 120 字符".into());
            }
            Ok(SurfaceElement::Progress { label: label.to_string(), value: value as f32 })
        },
        other => Err(format!("不支持的 UI 元素 type {other:?}")),
    }
}

/// 极简 markdown -> 纯文本：去掉标题/粗体/反引号/链接语法，列表保留项目符号。
///
/// 这是宿主渲染的最小实现：不解析 HTML、不执行链接、不加载远程图片。
pub fn render_markdown_plain(markdown: &str) -> String {
    let mut out = String::new();
    for line in markdown.lines() {
        let mut line = line.trim_end();
        if line.starts_with('#') {
            line = line.trim_start_matches('#').trim_start();
        }
        if let Some(rest) = line.strip_prefix("> ") {
            line = rest;
        }
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            out.push_str(indent);
            out.push_str("• ");
            line = rest;
        } else if let Some(rest) = trimmed.strip_prefix("+ ") {
            out.push_str(indent);
            out.push_str("• ");
            line = rest;
        }
        let line = replace_links(line);
        let line = line
            .replace("**", "")
            .replace("__", "")
            .replace("~~", "")
            .replace(['`', '*'], "")
            .replace("_", "");
        out.push_str(&line);
        out.push('\n');
    }
    if out.ends_with('\n') && !markdown.ends_with('\n') {
        out.pop();
    }
    clip_chars(&out, MAX_SURFACE_TEXT_CHARS)
}

fn replace_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(mid) = after.find("](") else {
            out.push_str(after);
            return out;
        };
        let Some(end) = after[mid + 2..].find(')') else {
            out.push_str(after);
            return out;
        };
        let text = &after[1..mid];
        let url = &after[mid + 2..mid + 2 + end];
        out.push_str(text);
        if !url.trim().is_empty() && url != text {
            out.push_str(" (");
            out.push_str(url);
            out.push(')');
        }
        rest = &after[mid + 3 + end..];
    }
    out.push_str(rest);
    out
}

/// 截断到最多 `max` 个字符（不是字节），避免拆开 UTF-8。
pub fn clip_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// `initialize` 响应必须声明 protocol_version = 3。
pub fn validate_initialize_result(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("initialize result 必须是 object")?;
    let version = object
        .get("protocol_version")
        .and_then(Value::as_u64)
        .ok_or("initialize result 缺少 protocol_version")?;
    if version != HOST_PROTOCOL_VERSION as u64 {
        return Err(format!(
            "initialize protocol_version = {version}，宿主支持 {}",
            HOST_PROTOCOL_VERSION
        ));
    }
    Ok(())
}

/// 事件通知 params；标题等由调用方先做长度限制。
pub fn event_params(event: PluginEvent, payload: &Value) -> Value {
    json!({
        "event": event.as_str(),
        "payload": payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_notification_response() {
        match parse_incoming(
            r#"{"jsonrpc":"2.0","id":1,"method":"context.version","params":{}}"#,
        ) {
            Incoming::Request { id, method, .. } => {
                assert_eq!(id.as_u64(), Some(1));
                assert_eq!(method, M_CONTEXT_VERSION);
            },
            other => panic!("{other:?}"),
        }
        match parse_incoming(r#"{"jsonrpc":"2.0","method":"ui.notify","params":{"text":"hi"}}"#) {
            Incoming::Notification { method, .. } => assert_eq!(method, M_UI_NOTIFY),
            other => panic!("{other:?}"),
        }
        match parse_incoming(r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#) {
            Incoming::Response { id, result, .. } => {
                assert_eq!(id, 7);
                assert!(result.is_some());
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bad_json_and_wrong_shapes_are_invalid() {
        assert!(matches!(parse_incoming("{nope"), Incoming::Invalid(_)));
        assert!(matches!(
            parse_incoming(r#"{"id":1,"result":{}}"#),
            Incoming::Invalid(_)
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":1,"method":"x","result":{}}"#),
            Incoming::Invalid(_)
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":"abc","result":{}}"#),
            Incoming::Invalid(_)
        ));
    }

    #[test]
    fn result_formatting_handles_ok_and_errors() {
        assert_eq!(format_command_result(&json!("hello")), (true, "hello".into()));
        assert_eq!(
            format_command_result(&json!({"ok":false,"message":"nope"})),
            (false, "nope".into())
        );
        let (_, text) = format_command_result(&json!({"ok":true}));
        assert!(text.contains("ok"));
    }

    #[test]
    fn status_and_notify_validation() {
        let status =
            parse_status_payload(&json!({"text":"running","tooltip":"t","command":"greet"}))
                .unwrap();
        assert_eq!(status.text, "running");
        assert_eq!(status.command.as_deref(), Some("greet"));
        assert!(parse_status_payload(&json!({"text":"x".repeat(MAX_STATUS_CHARS + 1)})).is_err());
        let notify = parse_notify_payload(&json!({"text":" hi ","level":"warn"})).unwrap();
        assert_eq!(notify.text, "hi");
        assert_eq!(notify.level, "warn");
        assert!(parse_notify_payload(&json!({"text":""})).is_err());
        assert!(parse_notify_payload(&json!({"text":"x","level":"fatal"})).is_err());
    }

    #[test]
    fn surface_doc_accepts_all_required_element_types() {
        let value = json!({
            "version": 1,
            "title": "Echo",
            "elements": [
                {"type": "text", "text": "hello"},
                {"type": "markdown", "text": "# Title\n**bold**"},
                {"type": "list", "items": ["a", "b"]},
                {"type": "input", "id": "name", "label": "Name", "value": "x"},
                {"type": "button", "id": "run", "label": "Run"},
                {"type": "select", "id": "mode", "options": ["a", "b"], "value": "b"},
                {"type": "progress", "value": 0.5, "label": "Loading"}
            ]
        });
        let doc = parse_surface_doc(&value).unwrap();
        assert_eq!(doc.elements.len(), 7);
        assert_eq!(doc.input_value("name"), Some("x"));
        assert_eq!(doc.next_select_value("mode").as_deref(), Some("a"));
        assert_eq!(
            render_markdown_plain("# Title\n**bold** and `code`"),
            "Title\nbold and code"
        );
    }

    #[test]
    fn surface_doc_rejects_unknown_or_oversized() {
        assert!(parse_surface_doc(&json!({"elements":[{"type":"html","text":"x"}]})).is_err());
        assert!(parse_surface_doc(&json!({"elements":[{"type":"input","id":"x"},{"type":"button","id":"x","label":"Y"}]})).is_err());
        let too_many: Vec<Value> = (0..MAX_SURFACE_ELEMENTS + 1)
            .map(|_| json!({"type":"text","text":"x"}))
            .collect();
        assert!(parse_surface_doc(&json!({"elements": too_many})).is_err());
        assert!(parse_surface_doc(
            &json!({"elements":[{"type":"select","id":"s","options":["a"],"value":"b"}]})
        )
        .is_err());
        assert!(parse_surface_doc(
            &json!({"elements":[{"type":"progress","value":1.5}]})
        )
        .is_err());
    }

    #[test]
    fn markdown_renderer_is_plain_text_only() {
        let rendered = render_markdown_plain(
            "# Heading\n- [link](https://example.com)\n> quote **bold**\n<script>x</script>",
        );
        assert!(rendered.contains("Heading"));
        assert!(rendered.contains("• link (https://example.com)"));
        assert!(rendered.contains("quote bold"));
        // 不转义/执行 HTML，只是原样文本。
        assert!(rendered.contains("<script>x</script>"));
    }

    #[test]
    fn initialize_requires_protocol_v3() {
        assert!(validate_initialize_result(&json!({"protocol_version":3})).is_ok());
        assert!(validate_initialize_result(&json!({"protocol_version":2})).is_err());
        assert!(validate_initialize_result(&json!({})).is_err());
    }

    #[test]
    fn terminal_params_are_clamped() {
        let limits = parse_terminal_read_params(&json!({"max_lines": 9999, "max_bytes": 0}));
        assert_eq!(limits.max_lines, MAX_TERMINAL_READ_LINES);
        assert_eq!(limits.max_bytes, 1);
        let defaults = parse_terminal_read_params(&json!({}));
        assert_eq!(defaults.max_lines, 80);
        assert_eq!(defaults.max_bytes, 32 * 1024);
    }
}

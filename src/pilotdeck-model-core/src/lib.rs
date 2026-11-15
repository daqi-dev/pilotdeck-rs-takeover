//! PilotDeck 模型协议中不依赖运行时的解析与规范化核心。
//!
//! 输入/输出使用 `pilotdeck-protocol` 的 UTF-16 `JsValue`，保持固定 TS 基线
//! 的缺省字段、孤立代理码元和数值语义。此 crate 不执行 HTTP、不保管凭据、
//! 不负责会话、重试调度或工具执行；四类协议的 Gateway 路径共用这些实现。
#![forbid(unsafe_code)]

pub mod anthropic;
pub mod arguments;
pub mod google;
pub mod jsonrepair;
pub mod openai;
pub mod request;

use pilotdeck_protocol::JsValue;

fn field<'a>(value: &'a JsValue, key: &str) -> &'a JsValue {
    static UNDEFINED: JsValue = JsValue::Undefined;
    value.get(key).unwrap_or(&UNDEFINED)
}

fn is(value: &JsValue, text: &str) -> bool {
    value.as_string().is_some_and(|s| s.equals_str(text))
}

pub mod codec;
pub mod driver;

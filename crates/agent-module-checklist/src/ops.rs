//! 清单操作：参数解析 + 纯状态迁移。
//!
//! 这一层不碰 IO、不拿锁：`run_read` 只读状态并产出结果，`run_write` 在
//! **候选状态副本**上做迁移（失败时副本直接丢弃，天然无副作用）。
//!
//! ## 版本语义
//!
//! - 每次成功的修改：`revision += 1`；
//! - `reset_session`：清空会话并换一个新 `epoch`（文件级），同时 revision+1；
//! - `expected_epoch` / `expected_revision` 可选；提供即校验，不匹配返回
//!   **明确的版本冲突错误**（附带当前值），绝不"猜一个位置继续改"。
//! - 旧的按位置修改（`check` / `uncheck` / `remove`）**必须**带
//!   `expected_revision`：没有版本号的 index 在并发下会勾错条目。

use serde_json::{json, Map, Value};
use uuid::Uuid;

use echo_defs::registry::ToolError;

use super::model::{self, ItemState, ListState, RedactorRef, SessionState, StoreState};

/// 写入目标：稳定 id 优先，名字用于兼容旧接口。
#[derive(Clone, Debug)]
pub(crate) enum Target {
    Id(String),
    Name(String),
}

/// 已解析的版本期望。
#[derive(Clone, Debug, Default)]
pub(crate) struct Expected {
    pub epoch: Option<String>,
    pub revision: Option<u64>,
}

/// 一次调用的解析结果。解析失败发生在拿写门之前，因此没有副作用。
#[derive(Debug)]
pub(crate) struct Request {
    pub operation: Operation,
    pub expected: Expected,
    /// `SHA-256(原始 request_id)`：原 key 可能是秘密，不持久化、不比较原文。
    pub request_key: Option<String>,
    /// `SHA-256(规范原始 payload JSON)`（去掉 `request_id` / `__session_id`）。
    ///
    /// 用**未脱敏**的原文计算：不同秘密即使脱敏后是同一个占位符也必须视为
    /// 不同请求；动态登记秘密也不会改变已确认请求的指纹。
    pub fingerprint: String,
}

#[derive(Debug)]
pub(crate) enum Operation {
    /// `list`：无目标 = 枚举全部；有目标 = 旧式 get。
    List {
        target: Option<Target>,
    },
    Get {
        target: Target,
    },
    Create {
        name: String,
        items: Vec<String>,
    },
    AddItems {
        target: Target,
        items: Vec<String>,
    },
    UpdateItem {
        target: Option<Target>,
        item_id: Option<String>,
        index: Option<usize>,
        text: Option<String>,
        done: Option<bool>,
    },
    RemoveItem {
        target: Option<Target>,
        item_id: Option<String>,
        index: Option<usize>,
    },
    Archive {
        target: Target,
        archived: bool,
    },
    Delete {
        target: Target,
    },
    ResetSession,
    Batch {
        operations: Vec<Operation>,
    },
    // ── 旧协议（临时兼容）──
    LegacyAdd {
        name: String,
        text: String,
    },
    LegacyCheck {
        name: String,
        index: usize,
        done: bool,
    },
    LegacyRemove {
        name: String,
        index: usize,
    },
    LegacyReset {
        name: String,
    },
}

impl Operation {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::List { .. } => "list",
            Self::Get { .. } => "get",
            Self::Create { .. } => "create",
            Self::AddItems { .. } => "add_items",
            Self::UpdateItem { .. } => "update_item",
            Self::RemoveItem { .. } => "remove_item",
            Self::Archive { .. } => "archive",
            Self::Delete { .. } => "delete",
            Self::ResetSession => "reset_session",
            Self::Batch { .. } => "batch",
            Self::LegacyAdd { .. } => "add",
            Self::LegacyCheck { done: true, .. } => "check",
            Self::LegacyCheck { done: false, .. } => "uncheck",
            Self::LegacyRemove { .. } => "remove",
            Self::LegacyReset { .. } => "reset",
        }
    }

    pub(crate) fn is_write(&self) -> bool {
        !matches!(self, Self::List { .. } | Self::Get { .. })
    }

    /// 是否按**位置**（`index`）修改。batch 递归判断，子操作不能绕过版本门。
    fn positional(&self) -> bool {
        match self {
            Self::UpdateItem { index: Some(_), .. }
            | Self::RemoveItem { index: Some(_), .. }
            | Self::LegacyCheck { .. }
            | Self::LegacyRemove { .. } => true,
            Self::Batch { operations } => operations.iter().any(Self::positional),
            _ => false,
        }
    }
}

/// 操作结果（尚未拼上 epoch/revision 的中间形态）。
#[derive(Debug)]
pub(crate) struct StepResult {
    pub changed: bool,
    pub summary: String,
    pub data: Map<String, Value>,
    pub display: Option<String>,
    /// 仅 reset_session：新的文件级 epoch。
    pub new_epoch: Option<String>,
}

impl StepResult {
    fn new(changed: bool, summary: impl Into<String>) -> Self {
        Self {
            changed,
            summary: summary.into(),
            data: Map::new(),
            display: None,
            new_epoch: None,
        }
    }
}

// ── 参数解析 ────────────────────────────────────────────────────────

pub(crate) fn parse_request(args: &Value, redactor: RedactorRef<'_>) -> Result<Request, ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| ToolError::InvalidArguments("checklist 参数必须是 JSON 对象".into()))?;
    let operation_name = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidArguments("缺少 operation".into()))?;

    let request_key = match object.get("request_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => {
            model::validate_request_id(id)?;
            // request_id 本身可能是秘密：命中当前脱敏器就拒绝；即使未命中，
            // 持久层也只保存 SHA-256 摘要，绝不落原 key。
            if let Some(redactor) = redactor {
                if redactor.redact(id).changed {
                    return Err(ToolError::InvalidArguments(
                        "request_id 不能包含已登记的秘密；请使用不含敏感内容的幂等键".into(),
                    ));
                }
            }
            Some(model::sha256_hex(id))
        }
        Some(_) => {
            return Err(ToolError::InvalidArguments(
                "request_id 必须是字符串".into(),
            ))
        }
    };

    let expected = parse_expected(object)?;
    let operation = parse_operation(operation_name, object, false)?;
    if operation.positional() && (expected.epoch.is_none() || expected.revision.is_none()) {
        return Err(ToolError::InvalidArguments(
            "按位置修改（index）必须同时提供 expected_epoch 和 expected_revision（先 list/get 取当前版本）；batch 中的位置子操作同样不允许绕过".into(),
        ));
    }

    let mut payload = args.clone();
    if let Some(map) = payload.as_object_mut() {
        // 会话由调用方显式传入（store.apply 的参数），不是 payload 的一部分。
        map.remove("__session_id");
        map.remove("request_id");
    }
    // 指纹必须基于**原始**规范 JSON：不脱敏，否则不同秘密会因同一个占位符
    // 被误判为同一请求；动态登记秘密后同一请求也会变成"冲突"。
    let fingerprint = model::payload_fingerprint(&payload);

    Ok(Request {
        operation,
        expected,
        request_key,
        fingerprint,
    })
}

fn parse_expected(object: &Map<String, Value>) -> Result<Expected, ToolError> {
    let mut expected = Expected::default();
    if let Some(value) = object.get("expected_epoch") {
        expected.epoch = Some(parse_epoch_value(value, "expected_epoch")?);
    }
    if let Some(value) = object.get("expected_revision") {
        expected.revision = Some(parse_revision_value(value, "expected_revision")?);
    }
    if let Some(value) = object.get("expected") {
        let nested = value.as_object().ok_or_else(|| {
            ToolError::InvalidArguments("expected 必须是对象 {epoch, revision}".into())
        })?;
        if expected.epoch.is_none() {
            if let Some(value) = nested.get("epoch") {
                expected.epoch = Some(parse_epoch_value(value, "expected.epoch")?);
            }
        }
        if expected.revision.is_none() {
            if let Some(value) = nested.get("revision") {
                expected.revision = Some(parse_revision_value(value, "expected.revision")?);
            }
        }
    }
    Ok(expected)
}

fn parse_epoch_value(value: &Value, field: &str) -> Result<String, ToolError> {
    let text = value
        .as_str()
        .ok_or_else(|| ToolError::InvalidArguments(format!("{field} 必须是 UUID 字符串")))?;
    Uuid::parse_str(text)
        .map_err(|_| ToolError::InvalidArguments(format!("{field} 必须是 UUID")))?;
    Ok(text.to_string())
}

fn parse_revision_value(value: &Value, field: &str) -> Result<u64, ToolError> {
    if let Some(number) = value.as_u64() {
        return Ok(number);
    }
    if let Some(text) = value.as_str() {
        return text
            .parse::<u64>()
            .map_err(|_| ToolError::InvalidArguments(format!("{field} 必须是非负整数")));
    }
    Err(ToolError::InvalidArguments(format!(
        "{field} 必须是非负整数"
    )))
}

fn parse_operation(
    name: &str,
    object: &Map<String, Value>,
    nested: bool,
) -> Result<Operation, ToolError> {
    match name {
        "list" => {
            reject_nested(nested, name)?;
            Ok(Operation::List {
                target: parse_optional_target(object)?,
            })
        }
        "get" => {
            reject_nested(nested, name)?;
            Ok(Operation::Get {
                target: parse_target(object, "default")?,
            })
        }
        "create" => {
            let name = parse_name(object, Some("default"))?;
            let items = parse_items(object, false, true)?;
            Ok(Operation::Create { name, items })
        }
        "add_items" => {
            let target = parse_target(object, "default")?;
            let items = parse_items(object, true, false)?;
            Ok(Operation::AddItems { target, items })
        }
        "update_item" => {
            let target = parse_optional_target(object)?;
            let item_id = parse_optional_uuid(object, "item_id")?;
            let index = parse_index(object)?;
            let text = parse_optional_string(object, "text")?;
            if let Some(text) = &text {
                model::validate_text(text)?;
            }
            let done = parse_optional_bool(object, "done")?;
            if text.is_none() && done.is_none() {
                return Err(ToolError::InvalidArguments(
                    "update_item 至少需要 text 或 done 之一".into(),
                ));
            }
            if item_id.is_none() && index.is_none() {
                return Err(ToolError::InvalidArguments(
                    "update_item 需要 item_id 或 index".into(),
                ));
            }
            if target.is_none() && item_id.is_none() {
                return Err(ToolError::InvalidArguments(
                    "update_item 未指定清单时必须提供 item_id".into(),
                ));
            }
            Ok(Operation::UpdateItem {
                target,
                item_id,
                index,
                text,
                done,
            })
        }
        "remove_item" => {
            let target = parse_optional_target(object)?;
            let item_id = parse_optional_uuid(object, "item_id")?;
            let index = parse_index(object)?;
            if item_id.is_none() && index.is_none() {
                return Err(ToolError::InvalidArguments(
                    "remove_item 需要 item_id 或 index".into(),
                ));
            }
            if target.is_none() && item_id.is_none() {
                return Err(ToolError::InvalidArguments(
                    "remove_item 未指定清单时必须提供 item_id".into(),
                ));
            }
            Ok(Operation::RemoveItem {
                target,
                item_id,
                index,
            })
        }
        "archive" => {
            let target = parse_target(object, "default")?;
            let archived = parse_optional_bool(object, "archived")?.unwrap_or(true);
            Ok(Operation::Archive { target, archived })
        }
        "delete" => Ok(Operation::Delete {
            target: parse_target(object, "default")?,
        }),
        "reset_session" => Ok(Operation::ResetSession),
        "batch" => {
            reject_nested(nested, name)?;
            parse_batch(object)
        }
        // ── 旧协议 ──
        "add" => {
            let name = parse_name(object, Some("default"))?;
            let text = parse_required_string(object, "item")?;
            model::validate_text(&text)?;
            Ok(Operation::LegacyAdd { name, text })
        }
        "check" | "uncheck" => {
            let list_name = parse_name(object, Some("default"))?;
            let index = parse_index(object)?
                .ok_or_else(|| ToolError::InvalidArguments(format!("{name} 需要 index")))?;
            Ok(Operation::LegacyCheck {
                name: list_name,
                index,
                done: name == "check",
            })
        }
        "remove" => {
            let name = parse_name(object, Some("default"))?;
            let index = parse_index(object)?
                .ok_or_else(|| ToolError::InvalidArguments("remove 需要 index".into()))?;
            Ok(Operation::LegacyRemove { name, index })
        }
        "reset" => Ok(Operation::LegacyReset {
            name: parse_name(object, Some("default"))?,
        }),
        other => Err(ToolError::InvalidArguments(format!(
            "未知操作: {other}（支持 list/get/create/add_items/update_item/remove_item/archive/delete/reset_session/batch，及旧式 add/check/uncheck/remove/reset）"
        ))),
    }
}

fn reject_nested(nested: bool, name: &str) -> Result<(), ToolError> {
    if nested {
        return Err(ToolError::InvalidArguments(format!(
            "batch 只支持写操作，不能嵌套 {name}"
        )));
    }
    Ok(())
}

fn parse_batch(object: &Map<String, Value>) -> Result<Operation, ToolError> {
    let operations = object
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::InvalidArguments("batch 需要 operations 数组".into()))?;
    if operations.is_empty() {
        return Err(ToolError::InvalidArguments("batch 至少需要一个操作".into()));
    }
    if operations.len() > model::MAX_BATCH_OPS {
        return Err(ToolError::InvalidArguments(format!(
            "batch 操作数超过上限 {}",
            model::MAX_BATCH_OPS
        )));
    }
    let mut parsed = Vec::with_capacity(operations.len());
    for raw in operations {
        let object = raw
            .as_object()
            .ok_or_else(|| ToolError::InvalidArguments("batch 的每个操作必须是对象".into()))?;
        let name = object
            .get("operation")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError::InvalidArguments("batch 子操作缺少 operation".into()))?;
        let operation = parse_operation(name, object, true)?;
        if !operation.is_write() {
            return Err(ToolError::InvalidArguments("batch 只支持写操作".into()));
        }
        parsed.push(operation);
    }
    Ok(Operation::Batch { operations: parsed })
}

fn parse_name(object: &Map<String, Value>, default: Option<&str>) -> Result<String, ToolError> {
    match object.get("name") {
        None | Some(Value::Null) => default
            .map(str::to_string)
            .ok_or_else(|| ToolError::InvalidArguments("缺少 name（清单名）".into())),
        Some(Value::String(name)) => {
            model::validate_name(name)?;
            Ok(name.clone())
        }
        Some(_) => Err(ToolError::InvalidArguments("name 必须是字符串".into())),
    }
}

fn parse_optional_string(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, ToolError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(ToolError::InvalidArguments(format!("{field} 必须是字符串"))),
    }
}

fn parse_required_string(object: &Map<String, Value>, field: &str) -> Result<String, ToolError> {
    parse_optional_string(object, field)?
        .ok_or_else(|| ToolError::InvalidArguments(format!("缺少 {field}")))
}

fn parse_optional_bool(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<bool>, ToolError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError::InvalidArguments(format!("{field} 必须是布尔值"))),
    }
}

fn parse_index(object: &Map<String, Value>) -> Result<Option<usize>, ToolError> {
    match object.get("index") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .ok_or_else(|| ToolError::InvalidArguments("index 必须是从 1 开始的整数".into()))?;
            let index = usize::try_from(number)
                .ok()
                .filter(|index| *index >= 1)
                .ok_or_else(|| ToolError::InvalidArguments("index 必须是从 1 开始的整数".into()))?;
            Ok(Some(index))
        }
    }
}

fn parse_optional_uuid(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, ToolError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) => {
            Uuid::parse_str(id)
                .map_err(|_| ToolError::InvalidArguments(format!("{field} 必须是 UUID")))?;
            Ok(Some(id.clone()))
        }
        Some(_) => Err(ToolError::InvalidArguments(format!(
            "{field} 必须是 UUID 字符串"
        ))),
    }
}

fn parse_optional_target(object: &Map<String, Value>) -> Result<Option<Target>, ToolError> {
    if let Some(id) = parse_optional_uuid(object, "list_id")? {
        return Ok(Some(Target::Id(id)));
    }
    Ok(parse_optional_string(object, "name")?
        .map(|name| {
            model::validate_name(&name)?;
            Ok(Target::Name(name))
        })
        .transpose()?)
}

fn parse_target(object: &Map<String, Value>, default_name: &str) -> Result<Target, ToolError> {
    Ok(parse_optional_target(object)?.unwrap_or_else(|| Target::Name(default_name.to_string())))
}

/// `items` 数组 + 单数 `item`（旧参数/便捷写法）。
fn parse_items(
    object: &Map<String, Value>,
    require_any: bool,
    allow_empty: bool,
) -> Result<Vec<String>, ToolError> {
    let mut items = Vec::new();
    match object.get("items") {
        None | Some(Value::Null) => {}
        Some(Value::Array(array)) => {
            if array.len() > model::MAX_ITEMS {
                return Err(ToolError::InvalidArguments(format!(
                    "items 数量超过单次上限 {}",
                    model::MAX_ITEMS
                )));
            }
            for value in array {
                let text = value
                    .as_str()
                    .ok_or_else(|| ToolError::InvalidArguments("items 必须是字符串数组".into()))?;
                items.push(text.to_string());
            }
        }
        Some(_) => return Err(ToolError::InvalidArguments("items 必须是字符串数组".into())),
    }
    match object.get("item") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => items.push(text.clone()),
        Some(_) => return Err(ToolError::InvalidArguments("item 必须是字符串".into())),
    }
    if items.is_empty() && require_any {
        return Err(ToolError::InvalidArguments(
            "需要 item 或 items（至少一项）".into(),
        ));
    }
    if items.is_empty() && !allow_empty {
        return Err(ToolError::InvalidArguments("items 不能为空".into()));
    }
    for text in &items {
        model::validate_text(text)?;
    }
    Ok(items)
}

// ── 读操作 ──────────────────────────────────────────────────────────

pub(crate) fn run_read(
    state: &StoreState,
    session_id: &str,
    operation: &Operation,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    match operation {
        Operation::List { target: None } => {
            let lists = lists_of(state, session_id);
            let summaries = lists
                .iter()
                .map(|list| model::list_summary_json(list, redactor))
                .collect::<Vec<_>>();
            let active = lists.iter().filter(|list| !list.archived).count();
            let mut step = StepResult::new(
                false,
                format!("共 {} 个清单（{} 个活动）", lists.len(), active),
            );
            step.data.insert("lists".into(), Value::Array(summaries));
            step.data.insert("session".into(), json!(session_id));
            Ok(step)
        }
        Operation::List {
            target: Some(target),
        }
        | Operation::Get { target } => match find_list(state, session_id, target, redactor) {
            Some(list) => {
                let mut step = StepResult::new(
                    false,
                    format!(
                        "清单 '{}'（{}/{} 完成）",
                        model::redact(redactor, &list.name),
                        list.items.iter().filter(|item| item.done).count(),
                        list.items.len()
                    ),
                );
                step.data.insert("exists".into(), json!(true));
                step.data
                    .insert("list".into(), model::list_json(list, redactor));
                step.display = Some(model::render_list(list, redactor));
                Ok(step)
            }
            None => {
                let label = target_label(target, redactor);
                let mut step = StepResult::new(
                    false,
                    format!("清单 '{label}' 不存在（先用 create/add_items 创建）"),
                );
                step.data.insert("exists".into(), json!(false));
                step.data.insert("list".into(), Value::Null);
                Ok(step)
            }
        },
        _ => Err(ToolError::InvalidArguments(format!(
            "{} 不是读操作",
            operation.name()
        ))),
    }
}

// ── 写操作 ──────────────────────────────────────────────────────────

pub(crate) fn run_write(
    state: &mut StoreState,
    session_id: &str,
    operation: &Operation,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    match operation {
        Operation::Create { name, items } => create_list(state, session_id, name, items, redactor),
        Operation::AddItems { target, items } => {
            add_items(state, session_id, target, items, redactor)
        }
        Operation::UpdateItem {
            target,
            item_id,
            index,
            text,
            done,
        } => update_item(
            state,
            session_id,
            target.as_ref(),
            item_id.as_deref(),
            *index,
            text.as_deref(),
            *done,
            redactor,
        ),
        Operation::RemoveItem {
            target,
            item_id,
            index,
        } => remove_item(
            state,
            session_id,
            target.as_ref(),
            item_id.as_deref(),
            *index,
            redactor,
        ),
        Operation::Archive { target, archived } => {
            archive_list(state, session_id, target, *archived, redactor)
        }
        Operation::Delete { target } => delete_list(state, session_id, target, redactor),
        Operation::ResetSession => reset_session(state, session_id, redactor),
        Operation::Batch { operations } => run_batch(state, session_id, operations, redactor),
        Operation::LegacyAdd { name, text } => add_items(
            state,
            session_id,
            &Target::Name(name.clone()),
            &[text.clone()],
            redactor,
        ),
        Operation::LegacyCheck { name, index, done } => {
            legacy_check(state, session_id, name, *index, *done, redactor)
        }
        Operation::LegacyRemove { name, index } => {
            legacy_remove(state, session_id, name, *index, redactor)
        }
        Operation::LegacyReset { name } => {
            delete_list(state, session_id, &Target::Name(name.clone()), redactor).map(|mut step| {
                // 旧文案保留（"已清空"），新 delete 已有等价语义。
                if step.changed {
                    step.summary = format!("清单 '{}' 已清空", model::redact(redactor, name));
                }
                step
            })
        }
        Operation::List { .. } | Operation::Get { .. } => Err(ToolError::InvalidArguments(
            format!("{} 不是写操作", operation.name()),
        )),
    }
}

fn lists_of<'a>(state: &'a StoreState, session_id: &str) -> &'a [ListState] {
    state
        .sessions
        .get(session_id)
        .map(|session| session.lists.as_slice())
        .unwrap_or(&[])
}

fn target_label(target: &Target, redactor: RedactorRef<'_>) -> String {
    match target {
        Target::Id(id) => id.clone(),
        Target::Name(name) => model::redact(redactor, name),
    }
}

fn find_list_index(
    lists: &[ListState],
    target: &Target,
    redactor: RedactorRef<'_>,
) -> Option<usize> {
    match target {
        Target::Id(id) => lists.iter().position(|list| &list.id == id),
        Target::Name(name) => lists
            .iter()
            .position(|list| &list.name == name)
            .or_else(|| {
                // 写入时名字已脱敏；查询名可能还是原文——两者都认。
                let redacted = model::redact(redactor, name);
                lists.iter().position(|list| list.name == redacted)
            }),
    }
}

fn find_list<'a>(
    state: &'a StoreState,
    session_id: &str,
    target: &Target,
    redactor: RedactorRef<'_>,
) -> Option<&'a ListState> {
    let lists = lists_of(state, session_id);
    find_list_index(lists, target, redactor).map(|index| &lists[index])
}

fn ensure_session<'a>(
    state: &'a mut StoreState,
    session_id: &str,
) -> Result<&'a mut SessionState, ToolError> {
    if !state.sessions.contains_key(session_id) {
        if state.sessions.len() >= model::MAX_SESSIONS {
            return Err(ToolError::InvalidArguments(format!(
                "会话数已达上限 {}，无法为新会话创建清单",
                model::MAX_SESSIONS
            )));
        }
        state
            .sessions
            .insert(session_id.to_string(), SessionState::default());
    }
    state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| ToolError::Execution("清单会话状态丢失".into()))
}

fn new_item(text: &str, redactor: RedactorRef<'_>, done: bool) -> Result<ItemState, ToolError> {
    let text = model::redact_for_store(redactor, text, model::MAX_TEXT_CHARS, "item 文本")?;
    Ok(ItemState {
        id: Uuid::new_v4().to_string(),
        text,
        done,
    })
}

fn list_step(
    changed: bool,
    summary: impl Into<String>,
    list: &ListState,
    redactor: RedactorRef<'_>,
) -> StepResult {
    let mut step = StepResult::new(changed, model::redact(redactor, &summary.into()));
    step.data.insert("exists".into(), json!(true));
    step.data
        .insert("list".into(), model::list_json(list, redactor));
    step.display = Some(model::render_list(list, redactor));
    step
}

fn create_list(
    state: &mut StoreState,
    session_id: &str,
    name: &str,
    items: &[String],
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = ensure_session(state, session_id)?;
    if let Some(index) = find_list_index(&session.lists, &Target::Name(name.to_string()), redactor)
    {
        let list = &session.lists[index];
        return Ok(list_step(
            false,
            format!(
                "清单 '{}' 已存在（保留原有条目；delete/reset 可清空）",
                model::redact(redactor, &list.name)
            ),
            list,
            redactor,
        ));
    }
    if session.lists.len() >= model::MAX_LISTS {
        return Err(ToolError::InvalidArguments(format!(
            "会话清单数已达上限 {}",
            model::MAX_LISTS
        )));
    }
    let mut new_items = Vec::with_capacity(items.len());
    for text in items {
        new_items.push(new_item(text, redactor, false)?);
    }
    let stored_name = model::redact_for_store(redactor, name, model::MAX_NAME_CHARS, "清单名")?;
    let list = ListState {
        id: Uuid::new_v4().to_string(),
        name: stored_name,
        archived: false,
        items: new_items,
    };
    session.lists.push(list);
    let list = session
        .lists
        .last()
        .ok_or_else(|| ToolError::Execution("清单创建失败".into()))?;
    Ok(list_step(
        true,
        format!(
            "清单 '{}' 已创建（{} 项）",
            model::redact(redactor, &list.name),
            list.items.len()
        ),
        list,
        redactor,
    ))
}

fn add_items(
    state: &mut StoreState,
    session_id: &str,
    target: &Target,
    items: &[String],
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    if items.is_empty() {
        return Err(ToolError::InvalidArguments("items 不能为空".into()));
    }
    let session = ensure_session(state, session_id)?;
    let index = match find_list_index(&session.lists, target, redactor) {
        Some(index) => index,
        None => match target {
            Target::Id(id) => {
                return Err(ToolError::InvalidArguments(format!(
                    "清单 id '{id}' 不存在（add_items 不按 id 自动建清单）"
                )))
            }
            Target::Name(name) => {
                if session.lists.len() >= model::MAX_LISTS {
                    return Err(ToolError::InvalidArguments(format!(
                        "会话清单数已达上限 {}",
                        model::MAX_LISTS
                    )));
                }
                let stored_name =
                    model::redact_for_store(redactor, name, model::MAX_NAME_CHARS, "清单名")?;
                session.lists.push(ListState {
                    id: Uuid::new_v4().to_string(),
                    name: stored_name,
                    archived: false,
                    items: Vec::new(),
                });
                session.lists.len() - 1
            }
        },
    };
    let list = &mut session.lists[index];
    if list.items.len() + items.len() > model::MAX_ITEMS {
        return Err(ToolError::InvalidArguments(format!(
            "清单 '{}' 项数将达 {}，超过上限 {}",
            model::redact(redactor, &list.name),
            list.items.len() + items.len(),
            model::MAX_ITEMS
        )));
    }
    let added = items.len();
    for text in items {
        list.items.push(new_item(text, redactor, false)?);
    }
    let total = list.items.len();
    Ok(list_step(
        true,
        format!(
            "已向清单 '{}' 添加 {added} 项（共 {total} 项）",
            model::redact(redactor, &list.name)
        ),
        list,
        redactor,
    ))
}

fn locate_item(
    session: &SessionState,
    target: Option<&Target>,
    item_id: Option<&str>,
    index: Option<usize>,
    redactor: RedactorRef<'_>,
) -> Result<(usize, usize), ToolError> {
    if let Some(target) = target {
        let list_index = find_list_index(&session.lists, target, redactor).ok_or_else(|| {
            ToolError::InvalidArguments(format!("清单 '{}' 不存在", target_label(target, redactor)))
        })?;
        let list = &session.lists[list_index];
        if let Some(id) = item_id {
            let item_index = list
                .items
                .iter()
                .position(|item| item.id == id)
                .ok_or_else(|| {
                    ToolError::InvalidArguments(format!(
                        "清单 '{}' 中没有 item id '{id}'",
                        model::redact(redactor, &list.name)
                    ))
                })?;
            return Ok((list_index, item_index));
        }
        if let Some(index) = index {
            if index == 0 || index > list.items.len() {
                return Err(ToolError::InvalidArguments(format!(
                    "清单 '{}' 没有第 {index} 项（共 {} 项）",
                    model::redact(redactor, &list.name),
                    list.items.len()
                )));
            }
            return Ok((list_index, index - 1));
        }
        return Err(ToolError::InvalidArguments("需要 item_id 或 index".into()));
    }
    let item_id = item_id
        .ok_or_else(|| ToolError::InvalidArguments("未指定清单时必须提供 item_id".into()))?;
    for (list_index, list) in session.lists.iter().enumerate() {
        if let Some(item_index) = list.items.iter().position(|item| item.id == item_id) {
            return Ok((list_index, item_index));
        }
    }
    Err(ToolError::InvalidArguments(format!(
        "当前会话没有 item id '{item_id}'"
    )))
}

#[allow(clippy::too_many_arguments)]
fn update_item(
    state: &mut StoreState,
    session_id: &str,
    target: Option<&Target>,
    item_id: Option<&str>,
    index: Option<usize>,
    text: Option<&str>,
    done: Option<bool>,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| ToolError::InvalidArguments("当前会话没有清单".into()))?;
    let (list_index, item_index) = locate_item(session, target, item_id, index, redactor)?;
    let list = &mut session.lists[list_index];
    let item = &mut list.items[item_index];
    let old_text = item.text.clone();
    let old_done = item.done;
    if let Some(text) = text {
        item.text = model::redact_for_store(redactor, text, model::MAX_TEXT_CHARS, "item 文本")?;
    }
    if let Some(done) = done {
        item.done = done;
    }
    let changed = item.text != old_text || item.done != old_done;
    let summary = if changed {
        format!(
            "清单 '{}' 第 {} 项已更新",
            model::redact(redactor, &list.name),
            item_index + 1
        )
    } else {
        format!(
            "清单 '{}' 第 {} 项无变化",
            model::redact(redactor, &list.name),
            item_index + 1
        )
    };
    Ok(list_step(changed, summary, list, redactor))
}

fn remove_item(
    state: &mut StoreState,
    session_id: &str,
    target: Option<&Target>,
    item_id: Option<&str>,
    index: Option<usize>,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| ToolError::InvalidArguments("当前会话没有清单".into()))?;
    let (list_index, item_index) = locate_item(session, target, item_id, index, redactor)?;
    let list = &mut session.lists[list_index];
    let removed = list.items.remove(item_index);
    let summary = format!(
        "已从清单 '{}' 删除 [{}] {}（剩余 {} 项）",
        model::redact(redactor, &list.name),
        item_index + 1,
        model::redact(redactor, &removed.text),
        list.items.len()
    );
    Ok(list_step(true, summary, list, redactor))
}

fn archive_list(
    state: &mut StoreState,
    session_id: &str,
    target: &Target,
    archived: bool,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = state
        .sessions
        .get_mut(session_id)
        .ok_or_else(|| ToolError::InvalidArguments("当前会话没有清单".into()))?;
    let index = find_list_index(&session.lists, target, redactor).ok_or_else(|| {
        ToolError::InvalidArguments(format!("清单 '{}' 不存在", target_label(target, redactor)))
    })?;
    let list = &mut session.lists[index];
    let changed = list.archived != archived;
    list.archived = archived;
    let summary = format!(
        "清单 '{}' 已{}",
        model::redact(redactor, &list.name),
        if archived { "归档" } else { "取消归档" }
    );
    Ok(list_step(changed, summary, list, redactor))
}

fn delete_list(
    state: &mut StoreState,
    session_id: &str,
    target: &Target,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let mut deleted: Option<ListState> = None;
    let mut became_empty = false;
    if let Some(session) = state.sessions.get_mut(session_id) {
        if let Some(index) = find_list_index(&session.lists, target, redactor) {
            deleted = Some(session.lists.remove(index));
            became_empty = session.lists.is_empty();
        }
    }
    if became_empty {
        state.sessions.remove(session_id);
    }
    let existed = deleted.is_some();
    let mut step = match deleted {
        Some(list) => {
            let mut step = StepResult::new(
                true,
                format!("清单 '{}' 已删除", model::redact(redactor, &list.name)),
            );
            step.data.insert("deleted".into(), json!(list.id));
            step.data.insert("list".into(), Value::Null);
            step
        }
        None => {
            let mut step = StepResult::new(
                false,
                format!("清单 '{}' 不存在", target_label(target, redactor)),
            );
            step.data.insert("deleted".into(), Value::Null);
            step.data.insert("list".into(), Value::Null);
            step
        }
    };
    step.data.insert("exists".into(), json!(existed));
    Ok(step)
}

fn reset_session(
    state: &mut StoreState,
    session_id: &str,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let cleared = state
        .sessions
        .remove(session_id)
        .map(|session| session.lists.len())
        .unwrap_or(0);
    let new_epoch = Uuid::new_v4().to_string();
    let mut step = StepResult::new(
        true,
        format!(
            "已重置会话 '{}' 的清单（清空 {} 个清单），并推进 epoch",
            model::redact(redactor, session_id),
            cleared
        ),
    );
    step.data.insert("reset".into(), json!(true));
    step.data.insert("cleared_lists".into(), json!(cleared));
    step.new_epoch = Some(new_epoch);
    Ok(step)
}

fn legacy_check(
    state: &mut StoreState,
    session_id: &str,
    name: &str,
    index: usize,
    done: bool,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = state.sessions.get_mut(session_id).ok_or_else(|| {
        ToolError::InvalidArguments(format!(
            "清单 '{}' 不存在（先用 add/create 创建）",
            model::redact(redactor, name)
        ))
    })?;
    let list_index = find_list_index(&session.lists, &Target::Name(name.to_string()), redactor)
        .ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "清单 '{}' 不存在（先用 add/create 创建）",
                model::redact(redactor, name)
            ))
        })?;
    let list = &mut session.lists[list_index];
    if index == 0 || index > list.items.len() {
        return Err(ToolError::InvalidArguments(format!(
            "清单 '{}' 没有第 {index} 项（共 {} 项）",
            model::redact(redactor, &list.name),
            list.items.len()
        )));
    }
    let item = &mut list.items[index - 1];
    let changed = item.done != done;
    item.done = done;
    let summary = if changed {
        format!(
            "清单 '{}' 第 {index} 项已{}",
            model::redact(redactor, &list.name),
            if done { "勾选" } else { "取消勾选" }
        )
    } else {
        format!(
            "清单 '{}' 第 {index} 项已是{}状态",
            model::redact(redactor, &list.name),
            if done { "勾选" } else { "未勾选" }
        )
    };
    Ok(list_step(changed, summary, list, redactor))
}

fn legacy_remove(
    state: &mut StoreState,
    session_id: &str,
    name: &str,
    index: usize,
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let session = state.sessions.get_mut(session_id).ok_or_else(|| {
        ToolError::InvalidArguments(format!("清单 '{}' 不存在", model::redact(redactor, name)))
    })?;
    let list_index = find_list_index(&session.lists, &Target::Name(name.to_string()), redactor)
        .ok_or_else(|| {
            ToolError::InvalidArguments(format!("清单 '{}' 不存在", model::redact(redactor, name)))
        })?;
    let list = &mut session.lists[list_index];
    if index == 0 || index > list.items.len() {
        return Err(ToolError::InvalidArguments(format!(
            "清单 '{}' 没有第 {index} 项（共 {} 项）",
            model::redact(redactor, &list.name),
            list.items.len()
        )));
    }
    let removed = list.items.remove(index - 1);
    let summary = format!(
        "已删除 [{index}] {}（剩余 {} 项）",
        model::redact(redactor, &removed.text),
        list.items.len()
    );
    Ok(list_step(true, summary, list, redactor))
}

fn run_batch(
    state: &mut StoreState,
    session_id: &str,
    operations: &[Operation],
    redactor: RedactorRef<'_>,
) -> Result<StepResult, ToolError> {
    let mut results = Vec::with_capacity(operations.len());
    let mut changed_count = 0usize;
    let mut new_epoch = None;
    for operation in operations {
        let step = run_write(state, session_id, operation, redactor)?;
        if step.changed {
            changed_count += 1;
        }
        if let Some(epoch) = step.new_epoch {
            new_epoch = Some(epoch);
        }
        results.push(json!({
            "operation": operation.name(),
            "changed": step.changed,
            "summary": step.summary,
        }));
    }
    let lists = lists_of(state, session_id)
        .iter()
        .map(|list| model::list_summary_json(list, redactor))
        .collect::<Vec<_>>();
    let mut step = StepResult::new(
        changed_count > 0,
        format!(
            "批量操作完成：{} 项，{} 项产生变更",
            operations.len(),
            changed_count
        ),
    );
    step.data.insert("results".into(), Value::Array(results));
    step.data.insert("lists".into(), Value::Array(lists));
    step.new_epoch = new_epoch;
    Ok(step)
}

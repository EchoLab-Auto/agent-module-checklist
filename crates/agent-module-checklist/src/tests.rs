//! Checklist store / tool 验收测试。
//!
//! 覆盖：重开恢复、坏文件不覆盖、写失败、并发版本冲突、batch 幂等、
//! 旧位置不误勾、reset epoch、查询/失败无副作用、假密钥三处不泄漏、
//! 限额边界、取消后磁盘/内存不分叉、取消等待写、跨 store/跨进程旧快照拒写。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use echo_defs::sanitize::Redactor;
use echo_defs::tool::{Cancellation, ToolContext};
use echo_test_utils::FakeRedactor;
use serde_json::{json, Value};
use uuid::Uuid;

use super::*;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "echo-checklist-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn file(&self) -> PathBuf {
        self.path.join("checklist.json")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

async fn call(store: &ChecklistStore, session: &str, args: Value) -> Value {
    let text = store
        .apply(session, args)
        .await
        .unwrap_or_else(|error| panic!("apply should succeed: {error}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("result must be JSON ({error}): {text}"))
}

async fn call_err(store: &ChecklistStore, session: &str, args: Value) -> ToolError {
    match store.apply(session, args).await {
        Ok(text) => panic!("expected error, got: {text}"),
        Err(error) => error,
    }
}

fn lists(value: &Value) -> &Vec<Value> {
    value["lists"]
        .as_array()
        .unwrap_or_else(|| panic!("lists must be an array: {value}"))
}

fn find_list<'a>(snapshot: &'a Value, name: &str) -> &'a Value {
    lists(snapshot)
        .iter()
        .find(|list| list["name"] == name)
        .unwrap_or_else(|| panic!("list {name:?} not found in {snapshot}"))
}

fn revision(tool: &ChecklistTool, session: &str) -> u64 {
    tool.snapshot_of(session).unwrap()["revision"]
        .as_u64()
        .expect("revision is u64")
}

fn epoch(tool: &ChecklistTool, session: &str) -> String {
    tool.snapshot_of(session).unwrap()["epoch"]
        .as_str()
        .expect("epoch is string")
        .to_string()
}

/// 可控取消信号（测试）：与宿主 `CancellationToken` 的薄包装同语义。
struct ManualCancellation {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl ManualCancellation {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

impl Cancellation for ManualCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn when_cancelled(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move { self.notify.notified().await })
    }
}

fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.is_file() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待文件超时: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── 重开恢复 ────────────────────────────────────────────────────────

#[tokio::test]
async fn reopen_restores_state_ids_epoch_and_revision() {
    let dir = TempDir::new("reopen");
    let store = ChecklistStore::open(dir.file(), None).unwrap();
    let created = call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a","b"]}),
    )
    .await;
    let list_id = created["list"]["id"].as_str().unwrap().to_string();
    let item_id = created["list"]["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let epoch = created["epoch"].as_str().unwrap().to_string();
    let rev = created["revision"].as_u64().unwrap();
    assert!(Uuid::parse_str(&epoch).is_ok());

    let updated = call(
        &store,
        "s1",
        json!({"operation":"update_item","list_id":list_id,"item_id":item_id,"done":true}),
    )
    .await;
    assert_eq!(updated["revision"].as_u64(), Some(rev + 1));

    // open 是只读加载：内容不被改写。
    let bytes_before = std::fs::read(dir.file()).unwrap();
    drop(store);
    let reopened = ChecklistStore::open(dir.file(), None).unwrap();
    assert_eq!(std::fs::read(dir.file()).unwrap(), bytes_before);

    let snapshot = reopened.snapshot("s1").unwrap();
    assert_eq!(snapshot["epoch"].as_str(), Some(epoch.as_str()));
    assert_eq!(snapshot["revision"].as_u64(), Some(rev + 1));
    let list = find_list(&snapshot, "work");
    assert_eq!(list["id"].as_str(), Some(list_id.as_str()));
    assert_eq!(list["items"][0]["id"].as_str(), Some(item_id.as_str()));
    assert_eq!(list["items"][0]["done"], json!(true));
    assert_eq!(list["total"].as_u64(), Some(2));

    let raw: Value = serde_json::from_slice(&std::fs::read(dir.file()).unwrap()).unwrap();
    assert_eq!(raw["version"], json!(1));
}

#[tokio::test]
async fn deleting_the_last_item_keeps_an_authoritative_empty_list() {
    let store = ChecklistStore::memory();
    let created = call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["only"]}),
    )
    .await;
    let list_id = created["list"]["id"].as_str().unwrap().to_string();
    let item_id = created["list"]["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    call(
        &store,
        "s1",
        json!({"operation":"remove_item","list_id":list_id,"item_id":item_id}),
    )
    .await;
    let snapshot = store.snapshot("s1").unwrap();
    let list = find_list(&snapshot, "work");
    assert_eq!(list["items"], json!([]));
    assert_eq!(list["total"].as_u64(), Some(0));

    // 空清单仍可被 list 枚举（上下文压缩后模型能找到它）。
    let enumerated = call(&store, "s1", json!({"operation":"list"})).await;
    assert_eq!(lists(&enumerated).len(), 1);
    assert_eq!(lists(&enumerated)[0]["name"], json!("work"));
}

#[tokio::test]
async fn open_is_read_only_and_leaves_no_tmp_files() {
    let dir = TempDir::new("readonly-open");
    let store = ChecklistStore::open(dir.file(), None).unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"add","name":"x","item":"one"}),
    )
    .await;
    drop(store);

    let before = std::fs::read(dir.file()).unwrap();
    let reopened = ChecklistStore::open(dir.file(), None).unwrap();
    assert_eq!(std::fs::read(dir.file()).unwrap(), before);
    drop(reopened);

    let entries: Vec<String> = std::fs::read_dir(&dir.path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "成功写入后只应有清单文件与 sidecar 锁，不应留下 tmp 文件: {entries:?}"
    );
    assert!(entries.iter().any(|name| name == "checklist.json"));
    assert!(
        entries.iter().any(|name| name.ends_with(".save-lock")),
        "磁盘写必须走 sidecar 写锁: {entries:?}"
    );
    assert!(
        !entries.iter().any(|name| name.contains(".tmp.")),
        "不应留下 tmp 文件: {entries:?}"
    );
}

// ── 坏文件不覆盖 ────────────────────────────────────────────────────

#[tokio::test]
async fn corrupt_or_unknown_files_are_rejected_without_overwrite() {
    let dir = TempDir::new("corrupt");
    let path = dir.file();

    let mut cases: Vec<Vec<u8>> = vec![
        b"{not json".to_vec(),
        json!({"version":2,"epoch":Uuid::new_v4().to_string(),"revision":0,"sessions":{}})
            .to_string()
            .into_bytes(),
        json!({
            "version":1,
            "epoch":Uuid::new_v4().to_string(),
            "revision":0,
            "sessions":{},
            "unknown_field":true
        })
        .to_string()
        .into_bytes(),
        json!({"version":1,"epoch":"not-a-uuid","revision":0,"sessions":{}})
            .to_string()
            .into_bytes(),
    ];
    // 33 个清单：超过每会话 32 上限，严格校验必须拒绝。
    let too_many_lists = (0..33)
        .map(|index| {
            json!({
                "id":Uuid::new_v4().to_string(),
                "name":format!("l{index}"),
                "archived":false,
                "items":[]
            })
        })
        .collect::<Vec<_>>();
    cases.push(
        json!({"version":1,"epoch":Uuid::new_v4().to_string(),"revision":0,
               "sessions":{"s1":{"lists":too_many_lists}}})
        .to_string()
        .into_bytes(),
    );

    for bytes in cases {
        std::fs::write(&path, &bytes).unwrap();
        let error = ChecklistStore::open(path.clone(), None)
            .expect_err("corrupt/unknown file must be rejected");
        assert!(
            format!("{error}").contains("清单文件"),
            "error should mention file: {error}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "open 失败后文件必须逐字节保持不变"
        );
    }

    // 超过 8 MiB 的文件在读之前就被拒绝。
    let oversized = vec![b' '; (model::MAX_FILE_BYTES + 1) as usize];
    std::fs::write(&path, &oversized).unwrap();
    let error = ChecklistStore::open(path.clone(), None).expect_err("oversized must be rejected");
    assert!(format!("{error}").contains("8 MiB"), "{error}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        oversized.len() as u64
    );
}

// ── 写失败 / 同步失败 ──────────────────────────────────────────────

#[tokio::test]
async fn write_failure_keeps_memory_and_can_retry() {
    let dir = TempDir::new("write-fail");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    let before = store.snapshot("s1").unwrap();

    // 用目录占住目标路径：rename(tmp, path) 必然失败。
    std::fs::create_dir(&path).unwrap();
    let error = call_err(&store, "s1", json!({"operation":"add","item":"x"})).await;
    assert!(format!("{error}").contains("保存失败"), "{error}");
    assert_eq!(
        store.snapshot("s1").unwrap(),
        before,
        "写失败不得发布内存状态"
    );
    assert_eq!(store.snapshot("s1").unwrap()["revision"].as_u64(), Some(0));
    let failed_entries: Vec<String> = std::fs::read_dir(&dir.path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        failed_entries.len(),
        2,
        "写失败后只应有目标路径与 sidecar 锁: {failed_entries:?}"
    );
    assert!(failed_entries.iter().any(|name| name == "checklist.json"));
    assert!(failed_entries
        .iter()
        .any(|name| name.ends_with(".save-lock")));
    assert!(
        !failed_entries.iter().any(|name| name.contains(".tmp.")),
        "写失败后必须清理 tmp 文件: {failed_entries:?}"
    );

    // 恢复后重试成功（同一进程内）。
    std::fs::remove_dir(&path).unwrap();
    let result = call(&store, "s1", json!({"operation":"add","item":"x"})).await;
    assert_eq!(result["changed"], json!(true));
    assert_eq!(result["revision"].as_u64(), Some(1));
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["text"],
        json!("x")
    );
    let reopened = ChecklistStore::open(path, None).unwrap();
    assert_eq!(
        reopened.snapshot("s1").unwrap()["lists"][0]["items"][0]["text"],
        json!("x")
    );
}

#[tokio::test]
async fn post_rename_sync_failure_is_explicit_but_published() {
    let dir = TempDir::new("dir-sync-fail");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    store.fail_next_dir_sync();

    let error = call_err(&store, "s1", json!({"operation":"add","item":"x"})).await;
    let message = format!("{error}");
    assert!(message.contains("目录同步失败"), "{message}");
    assert!(message.contains("已提交"), "{message}");

    // rename 已成功：磁盘有内容，内存也必须发布（不能假装没写过）。
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["text"],
        json!("x")
    );
    let persisted = std::fs::read_to_string(&path).unwrap();
    assert!(persisted.contains("\"x\""), "{persisted}");
    assert_eq!(store.snapshot("s1").unwrap()["revision"].as_u64(), Some(1));

    // 注入只影响下一次写；后续写正常。
    let result = call(&store, "s1", json!({"operation":"add","item":"y"})).await;
    assert_eq!(result["revision"].as_u64(), Some(2));
    let reopened = ChecklistStore::open(path, None).unwrap();
    let list = &reopened.snapshot("s1").unwrap()["lists"][0];
    assert_eq!(list["total"].as_u64(), Some(2));
}

// ── 并发版本冲突 ────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_writes_with_same_expected_revision_conflict() {
    let store = ChecklistStore::memory();
    let created = call(&store, "s1", json!({"operation":"create","name":"x"})).await;
    let list_id = created["list"]["id"].as_str().unwrap().to_string();
    let epoch = created["epoch"].as_str().unwrap().to_string();
    let rev = created["revision"].as_u64().unwrap();

    let first = store.apply(
        "s1",
        json!({"operation":"add_items","list_id":list_id,"items":["a"],
               "expected_epoch":epoch,"expected_revision":rev}),
    );
    let second = store.apply(
        "s1",
        json!({"operation":"add_items","list_id":list_id,"items":["b"],
               "expected_epoch":epoch,"expected_revision":rev}),
    );
    let (first, second) = tokio::join!(first, second);
    let error = match (first, second) {
        (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
        (Ok(_), Ok(_)) => panic!("两个同 revision 的写不该都成功"),
        (Err(a), Err(b)) => panic!("至少一个应成功: {a} / {b}"),
    };
    assert!(format!("{error}").contains("版本冲突"), "{error}");

    let snapshot = store.snapshot("s1").unwrap();
    assert_eq!(snapshot["revision"].as_u64(), Some(rev + 1));
    assert_eq!(snapshot["lists"][0]["total"].as_u64(), Some(1));
}

// ── batch + 幂等 ────────────────────────────────────────────────────

#[tokio::test]
async fn batch_retry_is_idempotent_and_rejects_other_payload() {
    let store = ChecklistStore::memory();
    let version = store.snapshot("s1").unwrap();
    let batch = json!({
        "operation":"batch",
        "request_id":"req-1",
        "expected_epoch":version["epoch"],
        "expected_revision":version["revision"],
        "operations":[
            {"operation":"create","name":"b","items":["a"]},
            {"operation":"add_items","name":"b","items":["c"]},
            {"operation":"update_item","name":"b","index":1,"done":true}
        ]
    });

    let first = store.apply("s1", batch.clone()).await.unwrap();
    let parsed: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(parsed["changed"], json!(true));
    assert_eq!(
        parsed["revision"].as_u64(),
        Some(1),
        "batch 内部多次修改只推进一次 revision"
    );
    let batch_list = parsed["lists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|list| list["name"] == "b")
        .unwrap();
    assert_eq!(batch_list["total"].as_u64(), Some(2));
    assert_eq!(batch_list["done"].as_u64(), Some(1));

    // 同 key 同 payload：逐字节同一结果，且不重复执行。
    let second = store.apply("s1", batch.clone()).await.unwrap();
    assert_eq!(second, first);
    let snapshot = store.snapshot("s1").unwrap();
    assert_eq!(snapshot["revision"].as_u64(), Some(1));
    assert_eq!(snapshot["lists"][0]["total"].as_u64(), Some(2));

    // 同 key 不同 payload：明确拒绝。
    let mut changed = batch.clone();
    changed["operations"][1]["items"] = json!(["z"]);
    let error = call_err(&store, "s1", changed).await;
    assert!(format!("{error}").contains("幂等键"), "{error}");

    // batch 中途失败：整批回滚（候选副本丢弃），不留下半成品。
    let version = store.snapshot("s1").unwrap();
    let bad = json!({
        "operation":"batch",
        "expected_epoch":version["epoch"],
        "expected_revision":version["revision"],
        "operations":[
            {"operation":"create","name":"z"},
            {"operation":"remove_item","name":"z","index":9}
        ]
    });
    assert!(store.apply("s1", bad).await.is_err());
    assert!(
        !lists(&store.snapshot("s1").unwrap())
            .iter()
            .any(|list| list["name"] == "z"),
        "失败的 batch 不得留下已创建清单"
    );
    assert_eq!(store.snapshot("s1").unwrap()["revision"].as_u64(), Some(1));
}

#[tokio::test]
async fn idempotency_survives_store_reopen() {
    let dir = TempDir::new("idem-reopen");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    let created = call(
        &store,
        "s",
        json!({"operation":"create","name":"work","items":["first"]}),
    )
    .await;
    let list_id = created["list"]["id"].clone();
    let request = json!({
        "operation":"add_items",
        "list_id":list_id,
        "items":["once"],
        "request_id":"durable-request-1"
    });
    let first = store.apply("s", request.clone()).await.unwrap();
    let before = store.snapshot("s").unwrap();
    drop(store);

    let reopened = ChecklistStore::open(path, None).unwrap();
    let retried = reopened.apply("s", request.clone()).await.unwrap();
    assert_eq!(retried, first, "重开后的同 key 同 payload 必须返回同一结果");
    assert_eq!(
        reopened.snapshot("s").unwrap(),
        before,
        "重开后的重试不得重复追加"
    );

    // 键顺序不同但语义相同的 payload：规范 JSON 指纹必须一致。
    let reordered: Value = serde_json::from_str(&format!(
        r#"{{"items":["once"],"request_id":"durable-request-1","operation":"add_items","list_id":{}}}"#,
        serde_json::to_string(&list_id).unwrap()
    ))
    .unwrap();
    let retried_reordered = reopened.apply("s", reordered).await.unwrap();
    assert_eq!(retried_reordered, first);

    // 同 key 不同 payload：重开之后同样明确拒绝。
    let mut changed = request;
    changed["items"] = json!(["twice"]);
    let error = call_err(&reopened, "s", changed).await;
    assert!(format!("{error}").contains("幂等键"), "{error}");
    assert_eq!(reopened.snapshot("s").unwrap(), before);
}

#[tokio::test]
async fn noop_ack_is_persisted_and_replays_after_reopen() {
    let dir = TempDir::new("idem-noop");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a"]}),
    )
    .await;

    // create 同名清单是 no-op（changed=false），但 ack 记录仍要落盘。
    let request = json!({"operation":"create","name":"work","request_id":"noop-1"});
    let first = store.apply("s1", request.clone()).await.unwrap();
    let parsed: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(parsed["changed"], json!(false));
    let before = store.snapshot("s1").unwrap();
    drop(store);

    let reopened = ChecklistStore::open(path, None).unwrap();
    let retried = reopened.apply("s1", request).await.unwrap();
    assert_eq!(retried, first, "no-op ack 重开后的重试必须返回同一结果");
    assert_eq!(
        reopened.snapshot("s1").unwrap(),
        before,
        "no-op ack 重试不得改变状态"
    );
}

#[tokio::test]
async fn idempotency_fingerprint_uses_raw_payload_not_redacted_placeholder() {
    let dir = TempDir::new("idem-raw-fingerprint");
    let redactor = Arc::new(FakeRedactor::new());
    redactor.register_secret("secret-alpha-1111", "alpha");
    redactor.register_secret("secret-beta-2222", "beta");
    let typed: Arc<dyn Redactor> = redactor.clone();
    let store = ChecklistStore::open(dir.file(), Some(typed)).unwrap();

    call(
        &store,
        "s1",
        json!({"operation":"add","item":"secret-alpha-1111","request_id":"same-key"}),
    )
    .await;
    assert_eq!(store.snapshot("s1").unwrap()["lists"][0]["total"], json!(1));

    // 两个不同秘密脱敏后都是同一个占位符；指纹基于原始 payload，必须冲突。
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"add","item":"secret-beta-2222","request_id":"same-key"}),
    )
    .await;
    assert!(format!("{error}").contains("幂等键"), "{error}");
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["total"],
        json!(1),
        "不同秘密不得被当成同一请求而重放"
    );
}

#[tokio::test]
async fn dynamic_secret_registration_does_not_break_acknowledged_retry() {
    let dir = TempDir::new("idem-dynamic-secret");
    let redactor = Arc::new(FakeRedactor::new());
    let typed: Arc<dyn Redactor> = redactor.clone();
    let store = ChecklistStore::open(dir.file(), Some(typed)).unwrap();
    let secret = "later-secret-4242";
    let request = json!({"operation":"add","item":secret,"request_id":"r-dyn"});

    let first = store.apply("s1", request.clone()).await.unwrap();
    assert!(first.contains(secret), "写入时该值尚未登记为秘密");

    // 动态登记秘密后，同 key 同 payload 仍然是同一请求：重放成功且结果脱敏。
    redactor.register_secret(secret, "late");
    let second = store.apply("s1", request).await.unwrap();
    assert!(!second.contains(secret), "重放结果必须动态脱敏: {second}");
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["total"],
        json!(1),
        "动态登记秘密不得把已确认请求变成冲突或重复执行"
    );

    // 下一次真实写会把旧值从文件和幂等记录里一并洗掉。
    call(&store, "s1", json!({"operation":"add","item":"plain"})).await;
    let file = std::fs::read_to_string(dir.file()).unwrap();
    assert!(!file.contains(secret), "文件泄漏: {file}");
}

#[tokio::test]
async fn secret_request_id_is_rejected_and_never_persisted() {
    let dir = TempDir::new("idem-secret-key");
    let redactor = Arc::new(FakeRedactor::new());
    redactor.register_secret("req-secret-9001", "request-id");
    let typed: Arc<dyn Redactor> = redactor.clone();
    let store = ChecklistStore::open(dir.file(), Some(typed)).unwrap();

    let error = call_err(
        &store,
        "s1",
        json!({"operation":"add","item":"x","request_id":"req-secret-9001"}),
    )
    .await;
    let message = format!("{error}");
    assert!(message.contains("request_id"), "{message}");
    assert!(!message.contains("req-secret-9001"), "{message}");
    assert!(!dir.file().exists(), "被拒绝的请求不得创建清单文件");
}

#[tokio::test]
async fn idempotency_records_persist_only_digests() {
    let dir = TempDir::new("idem-digests");
    let store = ChecklistStore::open(dir.file(), None).unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"add","item":"x","request_id":"durable-key-xyz"}),
    )
    .await;

    let file = std::fs::read_to_string(dir.file()).unwrap();
    assert!(
        !file.contains("durable-key-xyz"),
        "不得持久化原始 request_id: {file}"
    );
    assert!(!file.contains("\"payload\""), "不得持久化 payload: {file}");
    assert!(file.contains("\"key_digest\""), "{file}");
    assert!(file.contains("\"fingerprint\""), "{file}");
    let state: Value = serde_json::from_str(&file).unwrap();
    let record = &state["idempotency"]["s1"][0];
    assert_eq!(
        record["key_digest"].as_str().unwrap().len(),
        model::DIGEST_HEX_CHARS
    );
    assert_eq!(
        record["fingerprint"].as_str().unwrap().len(),
        model::DIGEST_HEX_CHARS
    );
    assert!(record["epoch"].is_string());
}

#[tokio::test]
async fn reset_session_rejects_replay_of_old_acknowledged_requests() {
    let store = ChecklistStore::memory();
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a"],"request_id":"before-reset"}),
    )
    .await;
    let before = store.snapshot("s1").unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"reset_session",
               "expected_epoch":before["epoch"],"expected_revision":before["revision"]}),
    )
    .await;
    assert_eq!(store.snapshot("s1").unwrap()["lists"], json!([]));

    // reset 之后重放旧请求：明确拒绝，绝不拿旧成功结果复活已清空的数据。
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a"],"request_id":"before-reset"}),
    )
    .await;
    assert!(format!("{error}").contains("幂等键"), "{error}");
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"],
        json!([]),
        "旧幂等结果不得在 reset 后复活数据"
    );
}

#[tokio::test]
async fn positional_entry_points_and_batch_require_epoch_and_revision() {
    let store = ChecklistStore::memory();
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a","b"]}),
    )
    .await;
    let version = store.snapshot("s1").unwrap();

    for args in [
        json!({"operation":"update_item","name":"work","index":1,"done":true}),
        json!({"operation":"remove_item","name":"work","index":1}),
        json!({"operation":"batch","operations":[
            {"operation":"update_item","name":"work","index":1,"done":true}
        ]}),
    ] {
        let error = call_err(&store, "s1", args).await;
        assert!(
            format!("{error}").contains("expected_epoch")
                && format!("{error}").contains("expected_revision"),
            "{error}"
        );
    }
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["done"],
        json!(false),
        "缺版本的位置修改不得生效"
    );

    // batch 不含位置子操作时无需版本（精确限制，不扩大破坏面）。
    call(
        &store,
        "s1",
        json!({"operation":"batch","operations":[
            {"operation":"add_items","name":"work","items":["c"]}
        ]}),
    )
    .await;

    let version = store.snapshot("s1").unwrap();
    let updated = call(
        &store,
        "s1",
        json!({"operation":"update_item","name":"work","index":1,"done":true,
               "expected_epoch":version["epoch"],"expected_revision":version["revision"]}),
    )
    .await;
    assert_eq!(updated["changed"], json!(true));

    let version = store.snapshot("s1").unwrap();
    let batched = call(
        &store,
        "s1",
        json!({"operation":"batch",
               "expected_epoch":version["epoch"],"expected_revision":version["revision"],
               "operations":[{"operation":"update_item","name":"work","index":2,"done":true}]}),
    )
    .await;
    assert_eq!(batched["changed"], json!(true));
}

#[tokio::test]
async fn post_rename_sync_failure_replays_without_reapplying() {
    let dir = TempDir::new("idem-post-rename");
    let store = ChecklistStore::open(dir.file(), None).unwrap();
    store.fail_next_dir_sync();
    let request = json!({"operation":"add","item":"x","request_id":"sync-fail-1"});

    let error = call_err(&store, "s1", request.clone()).await;
    let message = format!("{error}");
    assert!(message.contains("已提交"), "{message}");
    assert!(message.contains("目录同步失败"), "{message}");
    assert_eq!(store.snapshot("s1").unwrap()["lists"][0]["total"], json!(1));

    let retried = call(&store, "s1", request).await;
    assert_eq!(retried["changed"], json!(true));
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["total"],
        json!(1),
        "post-rename 失败后的重试不得重复执行"
    );
}

#[tokio::test]
async fn invalid_idempotency_records_are_rejected_on_load() {
    let dir = TempDir::new("idem-invalid");
    let path = dir.file();
    let epoch = Uuid::new_v4().to_string();
    let valid = "a".repeat(model::DIGEST_HEX_CHARS);
    let bytes = json!({
        "version":1,
        "epoch":epoch,
        "revision":0,
        "sessions":{},
        "idempotency":{"s1":[{
            "key_digest":"not-a-digest",
            "fingerprint":valid,
            "epoch":epoch,
            "outcome":{"Ok":{"result":{}}}
        }]}
    })
    .to_string()
    .into_bytes();
    std::fs::write(&path, &bytes).unwrap();
    let error = ChecklistStore::open(path.clone(), None).expect_err("bad digest must be rejected");
    assert!(format!("{error}").contains("清单文件"), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "加载失败不得覆盖文件");
}
#[tokio::test]
async fn idempotency_window_is_bounded_per_session() {
    let store = ChecklistStore::memory();
    for index in 0..105 {
        call(
            &store,
            "s1",
            json!({"operation":"add","item":format!("i{index}"),
                   "request_id":format!("r{index}")}),
        )
        .await;
    }
    assert_eq!(store.idem_count("s1"), model::MAX_IDEM_RECORDS);

    // 最早一条已被挤出窗口：同 key 会重新执行（而不是无限保留）。
    let total_before = store.snapshot("s1").unwrap()["lists"][0]["total"]
        .as_u64()
        .unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"add","item":"i0","request_id":"r0"}),
    )
    .await;
    let total_after = store.snapshot("s1").unwrap()["lists"][0]["total"]
        .as_u64()
        .unwrap();
    assert_eq!(total_after, total_before + 1);
}

// ── 旧位置修改不误勾 / ID 作用域 ────────────────────────────────────

#[tokio::test]
async fn legacy_positional_check_requires_revision_and_never_toggles_wrong_item() {
    let store = ChecklistStore::memory();
    call(&store, "s1", json!({"operation":"add","item":"a"})).await;
    call(&store, "s1", json!({"operation":"add","item":"b"})).await;
    let epoch = store.snapshot("s1").unwrap()["epoch"].clone();

    let error = call_err(&store, "s1", json!({"operation":"check","index":1})).await;
    assert!(format!("{error}").contains("expected_revision"), "{error}");
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["done"],
        json!(false),
        "缺版本号的 check 不得勾选"
    );

    // 只有 revision、没有 epoch：同样拒绝（按位置入口必须两个版本都在）。
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"check","index":1,"expected_revision":2}),
    )
    .await;
    assert!(format!("{error}").contains("expected_epoch"), "{error}");

    let error = call_err(
        &store,
        "s1",
        json!({"operation":"check","index":1,"expected_epoch":epoch,"expected_revision":1}),
    )
    .await;
    assert!(format!("{error}").contains("版本冲突"), "{error}");
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["done"],
        json!(false),
        "过期 revision 的 check 不得勾选"
    );

    call(
        &store,
        "s1",
        json!({"operation":"check","index":1,"expected_epoch":epoch,"expected_revision":2}),
    )
    .await;
    assert_eq!(
        store.snapshot("s1").unwrap()["lists"][0]["items"][0]["done"],
        json!(true)
    );
}

#[tokio::test]
async fn item_ids_are_scoped_to_their_list_and_session() {
    let store = ChecklistStore::memory();
    let first = call(
        &store,
        "s1",
        json!({"operation":"create","name":"l1","items":["x"]}),
    )
    .await;
    let first_id = first["list"]["id"].as_str().unwrap().to_string();
    let item_id = first["list"]["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let second = call(
        &store,
        "s1",
        json!({"operation":"create","name":"l2","items":["y"]}),
    )
    .await;
    let second_id = second["list"]["id"].as_str().unwrap().to_string();

    // 用另一个清单的 list_id 定位 item_id：必须报错而不是改到别处。
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"update_item","list_id":second_id,"item_id":item_id,"text":"hacked"}),
    )
    .await;
    assert!(format!("{error}").contains("没有 item id"), "{error}");
    assert_eq!(
        find_list(&store.snapshot("s1").unwrap(), "l1")["items"][0]["text"],
        json!("x")
    );

    // 跨会话的 item_id 不可见。
    let error = call_err(
        &store,
        "s2",
        json!({"operation":"update_item","item_id":item_id,"text":"hacked"}),
    )
    .await;
    assert!(format!("{error}").contains("当前会话没有"), "{error}");

    // 不带清单 id 的 item_id 修改在会话内可定位（稳定 id 的用途）。
    call(
        &store,
        "s1",
        json!({"operation":"update_item","item_id":item_id,"text":"x2"}),
    )
    .await;
    assert_eq!(
        find_list(&store.snapshot("s1").unwrap(), "l1")["items"][0]["text"],
        json!("x2")
    );
    call(
        &store,
        "s1",
        json!({"operation":"remove_item","list_id":first_id,"item_id":item_id}),
    )
    .await;
    assert_eq!(
        find_list(&store.snapshot("s1").unwrap(), "l1")["total"],
        json!(0)
    );
}

// ── reset_session / epoch ───────────────────────────────────────────

#[tokio::test]
async fn reset_session_advances_epoch_and_invalidates_old_expectations() {
    let dir = TempDir::new("reset-epoch");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    let first = call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":["a"]}),
    )
    .await;
    let second = call(&store, "s2", json!({"operation":"add","item":"b"})).await;
    let epoch = first["epoch"].as_str().unwrap().to_string();
    let rev = second["revision"].as_u64().unwrap();

    let reset = call(
        &store,
        "s1",
        json!({"operation":"reset_session","expected_epoch":epoch,"expected_revision":rev}),
    )
    .await;
    let new_epoch = reset["epoch"].as_str().unwrap().to_string();
    assert_ne!(new_epoch, epoch);
    assert_eq!(reset["revision"].as_u64(), Some(rev + 1));
    assert!(lists(&store.snapshot("s1").unwrap()).is_empty());
    let other = store.snapshot("s2").unwrap();
    assert_eq!(other["lists"][0]["items"][0]["text"], json!("b"));
    assert_eq!(other["epoch"].as_str(), Some(new_epoch.as_str()));

    // 旧 epoch 的写：明确冲突（文件级 epoch 已整体推进）。
    let error = call_err(
        &store,
        "s2",
        json!({"operation":"add","item":"c","expected_epoch":epoch,"expected_revision":rev}),
    )
    .await;
    assert!(format!("{error}").contains("版本冲突"), "{error}");

    drop(store);
    let reopened = ChecklistStore::open(path, None).unwrap();
    let snapshot = reopened.snapshot("s1").unwrap();
    assert_eq!(snapshot["epoch"].as_str(), Some(new_epoch.as_str()));
    assert!(lists(&snapshot).is_empty());
    assert_eq!(
        reopened.snapshot("s2").unwrap()["lists"][0]["items"][0]["text"],
        json!("b")
    );
}

// ── 查询 / 失败无副作用 ─────────────────────────────────────────────

#[tokio::test]
async fn reads_and_failed_writes_leave_state_unchanged() {
    let store = ChecklistStore::memory();
    let before = store.snapshot("s1").unwrap();
    let attempts = vec![
        json!({"operation":"list"}),
        json!({"operation":"get"}),
        json!({"operation":"get","name":"missing"}),
        json!({"operation":"list","name":"missing"}),
        json!({"operation":"check","index":1}),
        json!({"operation":"remove","index":0}),
        json!({"operation":"add"}),
        json!({"operation":"add","item":"   "}),
        json!({"operation":"update_item","name":"missing","index":1,"text":"x"}),
        json!({"operation":"remove_item","name":"missing","index":1}),
        json!({"operation":"bogus"}),
    ];
    for args in attempts {
        let _ = store.apply("s1", args).await;
        assert_eq!(
            store.snapshot("s1").unwrap(),
            before,
            "只读/失败调用不得改变状态"
        );
    }
    assert!(store.snapshots().is_empty(), "只读调用不得创建会话");
    assert!(store.summary("s1").is_none());

    call(&store, "s1", json!({"operation":"add","item":"keep"})).await;
    let before = store.snapshot("s1").unwrap();
    let rev = before["revision"].as_u64().unwrap();
    let epoch = before["epoch"].clone();
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"check","name":"absent","index":2,"expected_epoch":epoch,"expected_revision":rev}),
    )
    .await;
    assert!(format!("{error}").contains("不存在"), "{error}");
    assert_eq!(store.snapshot("s1").unwrap(), before);
}

// ── 脱敏 ────────────────────────────────────────────────────────────

#[tokio::test]
async fn secrets_are_redacted_in_file_snapshot_summary_and_results() {
    let dir = TempDir::new("secrets");
    let path = dir.file();
    let redactor = Arc::new(FakeRedactor::new());
    redactor.register_secret("sk-test-secret-123", "test");
    let typed: Arc<dyn Redactor> = redactor.clone();
    let store = ChecklistStore::open(path.clone(), Some(typed)).unwrap();
    let secret = "sk-test-secret-123";

    let result = call(
        &store,
        "s1",
        json!({"operation":"add","item":format!("token {secret}")}),
    )
    .await;
    assert!(!result.to_string().contains(secret), "结果泄漏: {result}");

    // 名称也含秘密 + 条目含引号/换行：返回、快照、文件、摘要都不能有明文，
    // 且 JSON 字段类型保持不变（仍是 string/bool/array，不因脱敏换类型）。
    call(
        &store,
        "s1",
        json!({
            "operation":"create",
            "name":format!("plan-{secret}"),
            "items":[format!("quote \" 换行\n and {secret}")]
        }),
    )
    .await;

    let snapshot = store.snapshot("s1").unwrap();
    let snapshot_text = snapshot.to_string();
    assert!(!snapshot_text.contains(secret), "快照泄漏: {snapshot_text}");
    assert!(snapshot_text.contains("已隐藏"), "{snapshot_text}");
    assert!(snapshot["epoch"].is_string());
    assert!(snapshot["revision"].is_u64());
    assert!(snapshot["lists"].is_array());
    for list in snapshot["lists"].as_array().unwrap() {
        assert!(list["name"].is_string());
        assert!(list["archived"].is_boolean());
        assert!(list["items"].is_array());
        for item in list["items"].as_array().unwrap() {
            assert!(item["text"].is_string());
            assert!(item["done"].is_boolean());
        }
    }

    let summary = store.summary("s1").unwrap();
    assert!(!summary.contains(secret), "摘要泄漏: {summary}");

    let file = std::fs::read_to_string(&path).unwrap();
    assert!(!file.contains(secret), "文件泄漏: {file}");
    assert!(file.contains("已隐藏"), "{file}");

    // 幂等记录 / 重放结果同样不能含秘密。
    let request = json!({"operation":"add","item":format!("again {secret}"),"request_id":"r1"});
    let first = store.apply("s1", request.clone()).await.unwrap();
    let second = store.apply("s1", request).await.unwrap();
    assert_eq!(first, second);
    assert!(!first.contains(secret));
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(!file.contains(secret), "幂等记录泄漏: {file}");
}

#[tokio::test]
async fn load_errors_never_echo_registered_secrets() {
    let dir = TempDir::new("load-error-redaction");
    let path = dir.file();
    let redactor = Arc::new(FakeRedactor::new());
    redactor.register_secret("sk-test-secret-123", "test");
    let typed: Arc<dyn Redactor> = redactor.clone();
    // 未知字段名本身是秘密：serde 的错误文案会回显字段名，出口必须脱敏。
    std::fs::write(
        &path,
        br#"{"version":1,"epoch":"00000000-0000-4000-8000-000000000000","revision":0,"sessions":{},"sk-test-secret-123":true}"#,
    )
    .unwrap();
    let error = ChecklistStore::open(path, Some(typed)).expect_err("unknown field must fail");
    let message = format!("{error}");
    assert!(!message.contains("sk-test-secret-123"), "{message}");
    assert!(message.contains("已隐藏"), "{message}");
}

#[tokio::test]
async fn redactor_is_dynamic_for_snapshot_and_next_write() {
    let dir = TempDir::new("dynamic-redactor");
    let path = dir.file();
    let redactor = Arc::new(FakeRedactor::new());
    let typed: Arc<dyn Redactor> = redactor.clone();
    let store = ChecklistStore::open(path.clone(), Some(typed)).unwrap();
    let secret = "dynamic-secret-9742";

    // 写入时脱敏器还不认识这个值：文件里暂时是原文（它不是"已知秘密"）。
    call(&store, "s1", json!({"operation":"add","item":secret})).await;
    assert!(std::fs::read_to_string(&path).unwrap().contains(secret));

    // 运行期登记后，快照出口立即脱敏（动态 Arc，而不是加载时快照）。
    redactor.register_secret(secret, "dynamic");
    let snapshot = store.snapshot("s1").unwrap().to_string();
    assert!(!snapshot.contains(secret), "{snapshot}");
    assert!(snapshot.contains("已隐藏"), "{snapshot}");

    // 下一次写前会逐字符串脱敏整个候选状态，把旧原文也洗掉。
    call(&store, "s1", json!({"operation":"add","item":"plain"})).await;
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(!file.contains(secret), "{file}");
    assert!(file.contains("已隐藏"), "{file}");
}

// ── 限额边界 ────────────────────────────────────────────────────────

#[tokio::test]
async fn limits_are_enforced() {
    // 文本 2000 / 2001
    let store = ChecklistStore::memory();
    call(
        &store,
        "s1",
        json!({"operation":"add","item":"t".repeat(model::MAX_TEXT_CHARS)}),
    )
    .await;
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"add","item":"t".repeat(model::MAX_TEXT_CHARS + 1)}),
    )
    .await;
    assert!(format!("{error}").contains("2000"), "{error}");

    // 清单名 128 / 129
    let store = ChecklistStore::memory();
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"n".repeat(model::MAX_NAME_CHARS)}),
    )
    .await;
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"create","name":"n".repeat(model::MAX_NAME_CHARS + 1)}),
    )
    .await;
    assert!(format!("{error}").contains("128"), "{error}");

    // 每会话 32 个清单 / 第 33 个
    let store = ChecklistStore::memory();
    for index in 0..model::MAX_LISTS {
        call(
            &store,
            "s1",
            json!({"operation":"create","name":format!("l{index}")}),
        )
        .await;
    }
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"create","name":"overflow"}),
    )
    .await;
    assert!(format!("{error}").contains("上限 32"), "{error}");

    // 每清单 256 项 / 第 257 项（create 的 items 一次建满）
    let store = ChecklistStore::memory();
    let items = (0..model::MAX_ITEMS)
        .map(|index| format!("i{index}"))
        .collect::<Vec<_>>();
    let created = call(
        &store,
        "s1",
        json!({"operation":"create","name":"big","items":items}),
    )
    .await;
    assert_eq!(created["list"]["total"].as_u64(), Some(256));
    let error = call_err(
        &store,
        "s1",
        json!({"operation":"add_items","name":"big","items":["one-more"]}),
    )
    .await;
    assert!(format!("{error}").contains("上限 256"), "{error}");

    // 每文件 256 个会话 / 第 257 个
    let store = ChecklistStore::memory();
    for index in 0..model::MAX_SESSIONS {
        call(
            &store,
            &format!("s{index}"),
            json!({"operation":"create","name":"l"}),
        )
        .await;
    }
    let error = call_err(
        &store,
        &format!("s{}", model::MAX_SESSIONS),
        json!({"operation":"create","name":"l"}),
    )
    .await;
    assert!(format!("{error}").contains("会话数已达上限"), "{error}");
}

// ── 概要 / 枚举 / 归档 ─────────────────────────────────────────────

#[tokio::test]
async fn list_enumerates_and_get_returns_full_data() {
    let store = ChecklistStore::memory();
    let first = call(
        &store,
        "s1",
        json!({"operation":"create","name":"l1","items":["a","b"]}),
    )
    .await;
    let list_id = first["list"]["id"].as_str().unwrap().to_string();
    call(&store, "s1", json!({"operation":"create","name":"l2"})).await;

    let all = call(&store, "s1", json!({"operation":"list"})).await;
    assert_eq!(all["changed"], json!(false));
    assert_eq!(lists(&all).len(), 2);
    for entry in lists(&all) {
        assert!(
            entry.get("items").is_none(),
            "枚举不携带完整 items: {entry}"
        );
        assert!(entry["id"].as_str().is_some());
        assert!(entry.get("archived").is_some());
    }

    let got = call(&store, "s1", json!({"operation":"get","list_id":list_id})).await;
    assert_eq!(got["exists"], json!(true));
    assert_eq!(got["list"]["items"].as_array().unwrap().len(), 2);
    assert!(got["display"].as_str().unwrap().contains("1. a"), "{got}");

    let by_name = call(&store, "s1", json!({"operation":"list","name":"l2"})).await;
    assert_eq!(by_name["exists"], json!(true));
    assert_eq!(by_name["list"]["name"], json!("l2"));

    let missing = call(&store, "s1", json!({"operation":"list","name":"nope"})).await;
    assert_eq!(missing["exists"], json!(false));
    assert!(missing["summary"].as_str().unwrap().contains("不存在"));
}

#[tokio::test]
async fn summary_only_active_undone_and_is_bounded() {
    let store = ChecklistStore::memory();
    let long = "很长的任务描述".repeat(250); // 约 1750 字符
    let mut work_items = vec![long.clone(); 10];
    work_items.push("short".to_string());
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"work","items":work_items}),
    )
    .await;
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"archived","items":["archived-todo"]}),
    )
    .await;
    call(
        &store,
        "s1",
        json!({"operation":"archive","name":"archived"}),
    )
    .await;
    let version = store.snapshot("s1").unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"update_item","name":"work","index":11,"done":true,
               "expected_epoch":version["epoch"],"expected_revision":version["revision"]}),
    )
    .await;

    let summary = store.summary("s1").unwrap();
    assert!(summary.contains("work"), "{summary}");
    assert!(
        !summary.contains("archived-todo"),
        "归档清单不该进摘要: {summary}"
    );
    assert!(summary.contains("不是指令"), "{summary}");
    assert!(
        summary.contains("已截断"),
        "超过 4000 字符必须截断: {summary}"
    );
    assert!(
        summary.chars().count() <= model::MAX_SUMMARY_CHARS + 8,
        "摘要超限: {}",
        summary.chars().count()
    );

    // 全部完成 -> None（不注入空摘要）。
    for index in 1..=11 {
        let version = store.snapshot("s1").unwrap();
        call(
            &store,
            "s1",
            json!({"operation":"update_item","name":"work","index":index,"done":true,
                   "expected_epoch":version["epoch"],"expected_revision":version["revision"]}),
        )
        .await;
    }
    assert!(store.summary("s1").is_none());

    let snapshot = store.snapshot("s1").unwrap();
    assert_eq!(find_list(&snapshot, "archived")["archived"], json!(true));
}

#[tokio::test]
async fn same_list_name_across_sessions_stays_isolated() {
    let dir = TempDir::new("cross-session");
    let path = dir.file();
    let store = ChecklistStore::open(path.clone(), None).unwrap();
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"shared","items":["A"]}),
    )
    .await;
    call(
        &store,
        "s2",
        json!({"operation":"add","name":"shared","item":"B"}),
    )
    .await;

    let s1 = store.snapshot("s1").unwrap();
    let s2 = store.snapshot("s2").unwrap();
    assert_eq!(find_list(&s1, "shared")["items"][0]["text"], json!("A"));
    assert_eq!(find_list(&s2, "shared")["items"][0]["text"], json!("B"));
    assert_eq!(find_list(&s1, "shared")["total"], json!(1));
    assert_eq!(find_list(&s2, "shared")["total"], json!(1));

    let snapshots = store.snapshots();
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots.iter().any(|(session, _)| session == "s1"));
    assert!(snapshots.iter().any(|(session, _)| session == "s2"));

    let full = store.full_snapshot();
    let sessions = full["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().any(|entry| entry["session"] == "s1"));
    assert!(sessions.iter().any(|entry| entry["session"] == "s2"));

    drop(store);
    let reopened = ChecklistStore::open(path, None).unwrap();
    assert_eq!(
        find_list(&reopened.snapshot("s1").unwrap(), "shared")["items"][0]["text"],
        json!("A")
    );
    assert_eq!(
        find_list(&reopened.snapshot("s2").unwrap(), "shared")["items"][0]["text"],
        json!("B")
    );
}

#[tokio::test]
async fn write_results_carry_epoch_revision_and_change_summary() {
    let store = ChecklistStore::memory();
    let result = call(&store, "s1", json!({"operation":"add","item":"x"})).await;
    assert_eq!(result["ok"], json!(true));
    assert_eq!(result["operation"], json!("add"));
    assert_eq!(result["changed"], json!(true));
    assert_eq!(result["revision"].as_u64(), Some(1));
    assert!(Uuid::parse_str(result["epoch"].as_str().unwrap()).is_ok());
    assert!(!result["summary"].as_str().unwrap().is_empty());
    assert_eq!(result["list"]["items"][0]["text"], json!("x"));

    let read = call(&store, "s1", json!({"operation":"list"})).await;
    assert_eq!(read["changed"], json!(false));
    assert_eq!(read["revision"].as_u64(), Some(1));

    // 摘要提供可操作身份：epoch / revision / list_id / item_id。
    let summary = store.summary("s1").unwrap();
    for identity in [
        result["epoch"].as_str().unwrap(),
        result["list"]["id"].as_str().unwrap(),
        result["list"]["items"][0]["id"].as_str().unwrap(),
    ] {
        assert!(summary.contains(identity), "摘要缺少 {identity}: {summary}");
    }
    assert!(summary.contains("revision"), "{summary}");
}

// ── 取消安全 ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_caller_does_not_lose_committed_state() {
    let dir = TempDir::new("cancel");
    let path = dir.file();
    let store = Arc::new(ChecklistStore::open(path.clone(), None).unwrap());
    let worker = Arc::clone(&store);
    let task = tokio::spawn(async move {
        worker
            .apply("s1", json!({"operation":"add","item":"survives"}))
            .await
    });
    // 给 apply 一次被 poll 的机会（内部 spawn 持有 Arc<Inner>）。
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    task.abort();
    let _ = task.await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if path.is_file() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "调用方取消后后台任务仍应完成磁盘提交"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    loop {
        let published = store.snapshot("s1").unwrap()["lists"]
            .as_array()
            .is_some_and(|lists| !lists.is_empty());
        if published {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "磁盘提交后内存必须被发布（不能靠调用 future）"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let reopened = ChecklistStore::open(path, None).unwrap();
    assert_eq!(
        reopened.snapshot("s1").unwrap()["lists"][0]["items"][0]["text"],
        json!("survives")
    );
}

#[tokio::test]
async fn cancelled_write_is_rejected_before_reading_state() {
    let store = ChecklistStore::memory();
    let cancel = Arc::new(ManualCancellation::new());
    cancel.cancel();
    let error = store
        .apply_with_cancel(
            "s1",
            json!({"operation":"create","name":"never","request_id":"cancelled-1"}),
            Some(cancel),
        )
        .await
        .expect_err("已取消的写必须明确失败");
    assert!(format!("{error}").contains("已取消"), "{error}");
    assert_eq!(store.snapshot("s1").unwrap()["lists"], json!([]));
    assert_eq!(store.idem_count("s1"), 0, "取消的写不得记录幂等");
}

#[tokio::test]
async fn checklist_tool_forwards_context_cancellation_to_store() {
    let tool = ChecklistTool::new();
    let cancel = Arc::new(ManualCancellation::new());
    cancel.cancel();
    let context = ToolContext {
        session_id: "s1".into(),
        cancel: Some(cancel),
        ..Default::default()
    };
    let error = tool
        .execute_in(&context, json!({"operation":"add","item":"never"}))
        .await
        .expect_err("工具必须把上下文取消信号转发给 store");
    assert!(format!("{error}").contains("已取消"), "{error}");
    assert_eq!(tool.snapshot_of("s1").unwrap()["lists"], json!([]));
}

/// 关键回归：旧写已经 spawn、正在等写门；调用方取消后 reset 先提交；
/// 旧写醒来时必须被取消闸门拦住，不能在 reset 之后把 ghost 项复活。
///
/// 用 `hold_next_write_gate` / `notify_next_gate_wait` 固定排队顺序：
/// blocker 持门 → reset 排队 → 取消写排队 → 取消并放行。
#[tokio::test]
async fn cancelled_write_waiting_for_gate_cannot_revive_after_reset() {
    let dir = TempDir::new("cancel-waiting");
    let path = dir.file();
    let store = Arc::new(ChecklistStore::open(path.clone(), None).unwrap());
    let session = "s1";

    let (entered, release) = store.hold_next_write_gate();
    let blocker = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            store
                .apply(session, json!({"operation":"add","item":"before-reset"}))
                .await
        }
    });
    entered.notified().await;

    // reset 先排队（它是 blocker 之后的第一个等门者）。
    let reset_wait = store.notify_next_gate_wait();
    let reset = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            store
                .apply(session, json!({"operation":"reset_session"}))
                .await
        }
    });
    reset_wait.notified().await;

    // 取消写排在 reset 之后；它醒得最晚，正好模拟"取消后、reset 后执行"。
    let cancel = Arc::new(ManualCancellation::new());
    let write_wait = store.notify_next_gate_wait();
    let cancelled = tokio::spawn({
        let store = Arc::clone(&store);
        let cancel = Arc::clone(&cancel);
        async move {
            store
                .apply_with_cancel(
                    session,
                    json!({"operation":"add","item":"ghost","request_id":"ghost-1"}),
                    Some(cancel),
                )
                .await
        }
    });
    write_wait.notified().await;
    cancel.cancel();
    release.add_permits(1);

    blocker
        .await
        .expect("blocker task")
        .expect("blocker write must succeed");
    reset
        .await
        .expect("reset task")
        .expect("reset must complete before the cancelled writer");
    let error = cancelled
        .await
        .expect("cancelled task")
        .expect_err("等待写门的取消写必须明确失败，而不是在 reset 后执行");
    assert!(format!("{error}").contains("已取消"), "{error}");

    assert_eq!(
        store.snapshot(session).unwrap()["lists"],
        json!([]),
        "reset 后不得复活被取消的写"
    );
    assert_eq!(store.idem_count(session), 0, "取消的写不得记录幂等");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("ghost"), "磁盘不得出现取消写的候选: {raw}");
}

// ── 跨 store / 跨进程写者 ──────────────────────────────────────────

#[tokio::test]
async fn stale_store_cannot_overwrite_newer_state() {
    let dir = TempDir::new("stale-store");
    let path = dir.file();
    // 两个 store 在同一路径、同一（缺席）基线上打开。
    let older = ChecklistStore::open(path.clone(), None).unwrap();
    let newer = ChecklistStore::open(path.clone(), None).unwrap();
    call(
        &newer,
        "s1",
        json!({"operation":"create","name":"newer","items":["kept"]}),
    )
    .await;
    let newer_snapshot = newer.snapshot("s1").unwrap();

    let error = call_err(
        &older,
        "s1",
        json!({"operation":"create","name":"older","items":["lost"],"request_id":"older-1"}),
    )
    .await;
    let message = format!("{error}");
    assert!(
        message.contains("其他写者") && message.contains("重新加载"),
        "旧 store 必须明确 Stale 而不是静默覆盖: {message}"
    );
    assert!(
        !message.contains("lost") && !message.contains("kept"),
        "Stale 错误不得回显候选/磁盘内容: {message}"
    );
    assert_eq!(
        older.snapshot("s1").unwrap()["lists"],
        json!([]),
        "失败的旧候选不得发布到内存"
    );
    assert_eq!(
        older.idem_count("s1"),
        0,
        "Stale 拒写不得记录幂等（否则重试会假装成功）"
    );

    let reopened = ChecklistStore::open(path.clone(), None).unwrap();
    assert_eq!(
        reopened.snapshot("s1").unwrap(),
        newer_snapshot,
        "磁盘必须仍然是新写者的状态"
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("older"), "旧快照不得写入磁盘: {raw}");

    // reopen 后（新基线）正常串行写仍可用。
    call(
        &reopened,
        "s1",
        json!({"operation":"add","name":"newer","item":"after-reopen"}),
    )
    .await;
    let final_snapshot = ChecklistStore::open(path, None)
        .unwrap()
        .snapshot("s1")
        .unwrap();
    assert_eq!(
        find_list(&final_snapshot, "newer")["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stores_on_same_path_have_exactly_one_winner() {
    let dir = TempDir::new("concurrent-stores");
    let path = dir.file();
    {
        let seed = ChecklistStore::open(path.clone(), None).unwrap();
        call(&seed, "s1", json!({"operation":"create","name":"base"})).await;
    }
    let a = Arc::new(ChecklistStore::open(path.clone(), None).unwrap());
    let b = Arc::new(ChecklistStore::open(path.clone(), None).unwrap());
    let baseline = a.snapshot("s1").unwrap();
    assert_eq!(b.snapshot("s1").unwrap(), baseline);

    let task_a = tokio::spawn({
        let a = Arc::clone(&a);
        async move {
            a.apply("s1", json!({"operation":"create","name":"from-a"}))
                .await
        }
    });
    let task_b = tokio::spawn({
        let b = Arc::clone(&b);
        async move {
            b.apply("s1", json!({"operation":"create","name":"from-b"}))
                .await
        }
    });
    let (result_a, result_b) = tokio::join!(task_a, task_b);
    let result_a = result_a.expect("task a");
    let result_b = result_b.expect("task b");
    assert!(
        result_a.is_ok() ^ result_b.is_ok(),
        "同一路径的两个并发写必须恰好一个成功（另一个明确 Stale）: {result_a:?} / {result_b:?}"
    );
    let loser_error = if let Err(error) = &result_a {
        format!("{error}")
    } else {
        format!("{}", result_b.as_ref().unwrap_err())
    };
    assert!(loser_error.contains("其他写者"), "{loser_error}");

    let reopened = ChecklistStore::open(path, None).unwrap();
    let names: Vec<String> = lists(&reopened.snapshot("s1").unwrap())
        .iter()
        .map(|list| list["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names.len(), 2, "基线清单 + 唯一赢家: {names:?}");
    assert!(names.contains(&"base".to_string()));
    assert!(
        names.contains(&"from-a".to_string()) ^ names.contains(&"from-b".to_string()),
        "只能有赢家的清单: {names:?}"
    );
    // 输家的内存仍是旧基线（没有发布失败候选）。
    let loser_snapshot = if result_a.is_err() {
        a.snapshot("s1").unwrap()
    } else {
        b.snapshot("s1").unwrap()
    };
    assert_eq!(loser_snapshot, baseline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disk_writes_wait_for_the_sidecar_path_lock() {
    let dir = TempDir::new("lock-wait");
    let path = dir.file();
    let store = Arc::new(ChecklistStore::open(path.clone(), None).unwrap());
    let lock_path = super::store::lock_path(&path);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    held.try_lock().expect("测试必须先持有 sidecar 锁");

    let writer = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            store
                .apply("s1", json!({"operation":"create","name":"blocked"}))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !writer.is_finished(),
        "磁盘写必须等待 sidecar 锁，不能绕过锁直接 rename"
    );

    drop(held);
    let result = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("释放锁后写者应完成")
        .expect("writer task");
    result.expect("写者拿到锁后应成功");
    assert_eq!(
        find_list(&store.snapshot("s1").unwrap(), "blocked")["name"],
        json!("blocked")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cross_process_stale_writer_is_rejected() {
    let dir = TempDir::new("cross-process");
    let path = dir.file();
    let ready = dir.path.join("child-ready");
    let go = dir.path.join("child-go");
    let store = ChecklistStore::open(path.clone(), None).unwrap();

    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "tool::tests::cross_process_child_rejects_stale_write",
            "--ignored",
            "--nocapture",
        ])
        .env("ECHO_CHECKLIST_CHILD_PATH", &path)
        .env("ECHO_CHECKLIST_CHILD_READY", &ready)
        .env("ECHO_CHECKLIST_CHILD_GO", &go)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn child test process");

    wait_for_file(&ready, Duration::from_secs(15));
    // 父进程先提交；子进程的基线仍是"文件不存在"。
    call(
        &store,
        "s1",
        json!({"operation":"create","name":"parent-new","items":["kept"]}),
    )
    .await;
    std::fs::write(&go, b"go").expect("release child");
    let status = child.wait_with_output().expect("wait child");
    assert!(
        status.status.success(),
        "子进程必须证明跨进程旧写被拒（stdout: {}; stderr: {}）",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    let reopened = ChecklistStore::open(path, None).unwrap();
    let names: Vec<String> = lists(&reopened.snapshot("s1").unwrap())
        .iter()
        .map(|list| list["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"parent-new".to_string()));
    assert!(
        !names.contains(&"child-stale".to_string()),
        "跨进程旧快照不得覆盖父进程的新状态: {names:?}"
    );
}

/// 子进程角色：由 [`cross_process_stale_writer_is_rejected`] 以 `--ignored
/// --exact` 启动；没有环境变量时（普通测试运行）直接返回。
#[cfg(unix)]
#[test]
#[ignore = "child process role of cross_process_stale_writer_is_rejected"]
fn cross_process_child_rejects_stale_write() {
    let Ok(path) = std::env::var("ECHO_CHECKLIST_CHILD_PATH") else {
        return;
    };
    let ready = PathBuf::from(std::env::var("ECHO_CHECKLIST_CHILD_READY").expect("child ready"));
    let go = PathBuf::from(std::env::var("ECHO_CHECKLIST_CHILD_GO").expect("child go"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("child runtime");
    runtime.block_on(async move {
        let store = ChecklistStore::open(PathBuf::from(path), None).expect("child open");
        std::fs::write(&ready, b"ready").expect("child signals ready");
        wait_for_file(&go, Duration::from_secs(15));
        let error = store
            .apply("s1", json!({"operation":"create","name":"child-stale"}))
            .await
            .expect_err("子进程旧基线写必须被拒绝");
        let message = format!("{error}");
        assert!(
            message.contains("其他写者") && message.contains("重新加载"),
            "跨进程旧写必须明确 Stale: {message}"
        );
    });
}

// ── 工具接线 / 旧测试（按新协议更新，保留验收意义）─────────────────

#[tokio::test]
async fn checklist_tool_prefers_context_session_and_keeps_fallback() {
    let tool = ChecklistTool::with_store(Arc::new(ChecklistStore::memory()));
    let context = ToolContext {
        session_id: "ctx:1".into(),
        ..Default::default()
    };
    tool.execute_in(
        &context,
        json!({"operation":"add","item":"from-context","__session_id":"old"}),
    )
    .await
    .unwrap();
    let context_snapshot = tool.snapshot_for_session("ctx:1").unwrap();
    assert_eq!(
        find_list(&context_snapshot, "default")["items"][0]["text"],
        json!("from-context")
    );
    assert!(
        lists(&tool.snapshot_for_session("old").unwrap()).is_empty(),
        "上下文会话优先，不得写到 __session_id"
    );

    tool.execute(json!({"operation":"add","item":"from-old","__session_id":"old"}))
        .await
        .unwrap();
    let old_snapshot = tool.snapshot_for_session("old").unwrap();
    assert_eq!(
        find_list(&old_snapshot, "default")["items"][0]["text"],
        json!("from-old")
    );

    tool.execute(json!({"operation":"add","item":"fallback"}))
        .await
        .unwrap();
    let fallback = tool.snapshot_for_session(FALLBACK_SESSION).unwrap();
    assert_eq!(
        find_list(&fallback, "default")["items"][0]["text"],
        json!("fallback")
    );

    let full = tool.state_snapshot().unwrap();
    assert!(
        full["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["session"] == "ctx:1"),
        "全量调试快照必须保留来源: {full}"
    );
    assert_eq!(tool.state_key(), Some("checklist"));
}

#[tokio::test]
async fn add_and_list() {
    let t = ChecklistTool::new();
    let out = t
        .execute(json!({"operation": "add", "item": "写代码"}))
        .await
        .unwrap();
    assert!(out.contains("[ ] 1. 写代码"), "got: {out}");
    let out = t
        .execute(json!({"operation": "add", "item": "测试"}))
        .await
        .unwrap();
    assert!(out.contains("[ ] 2. 测试"), "got: {out}");
}

#[tokio::test]
async fn check_uncheck_remove() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation": "add", "item": "a"}))
        .await
        .unwrap();
    t.execute(json!({"operation": "add", "item": "b"}))
        .await
        .unwrap();
    let out = t
        .execute(json!({"operation": "check", "index": 1,
                        "expected_epoch": epoch(&t, FALLBACK_SESSION),
                        "expected_revision": revision(&t, FALLBACK_SESSION)}))
        .await
        .unwrap();
    assert!(out.contains("[x] 1. a"), "got: {out}");
    assert!(out.contains("1/2 完成"), "got: {out}");
    let out = t
        .execute(json!({"operation": "uncheck", "index": 1,
                        "expected_epoch": epoch(&t, FALLBACK_SESSION),
                        "expected_revision": revision(&t, FALLBACK_SESSION)}))
        .await
        .unwrap();
    assert!(out.contains("[ ] 1. a"), "got: {out}");
    let out = t
        .execute(json!({"operation": "remove", "index": 1,
                        "expected_epoch": epoch(&t, FALLBACK_SESSION),
                        "expected_revision": revision(&t, FALLBACK_SESSION)}))
        .await
        .unwrap();
    assert!(out.contains("已删除"), "got: {out}");
    assert!(!out.contains("1. a"), "got: {out}");
}
#[tokio::test]
async fn named_lists_are_independent() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation": "add", "name": "部署", "item": "构建"}))
        .await
        .unwrap();
    let out = t
        .execute(json!({"operation": "list", "name": "default"}))
        .await
        .unwrap();
    // 只读 list 不造幽灵空清单：不存在即报不存在——依然证明 default 与
    // 命名清单隔离（没有混入「构建」）。
    assert!(out.contains("不存在"), "default 应与命名清单隔离: {out}");
    let out = t
        .execute(json!({"operation": "list", "name": "部署"}))
        .await
        .unwrap();
    assert!(out.contains("1. 构建"), "got: {out}");
}

#[tokio::test]
async fn errors() {
    let t = ChecklistTool::new();
    assert!(t.execute(json!({})).await.is_err(), "缺少 operation");
    assert!(
        t.execute(json!({"operation": "add"})).await.is_err(),
        "add 缺少 item"
    );
    assert!(
        t.execute(json!({"operation": "check", "index": 5}))
            .await
            .is_err(),
        "缺 expected_revision 的按位置修改"
    );
    assert!(
        t.execute(json!({"operation": "check", "index": 0,
                         "expected_epoch": epoch(&t, FALLBACK_SESSION),
                         "expected_revision": 0}))
            .await
            .is_err(),
        "index 从 1 开始"
    );
    assert!(
        t.execute(json!({"operation": "bogus"})).await.is_err(),
        "未知操作"
    );
}

#[tokio::test]
async fn reset_clears() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation": "add", "item": "a"}))
        .await
        .unwrap();
    let out = t.execute(json!({"operation": "reset"})).await.unwrap();
    assert!(out.contains("已清空"), "got: {out}");
    let out: Value =
        serde_json::from_str(&t.execute(json!({"operation": "list"})).await.unwrap()).unwrap();
    assert_eq!(out["lists"], json!([]));
}

/// 会话隔离（2026-10 P0 修复）：不同 `__session_id` 的清单互不可见、
/// 互不覆盖；快照也按会话过滤。
#[tokio::test]
async fn sessions_are_isolated() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation": "add", "item": "A-1", "__session_id": "local:tui::local_user"}))
        .await
        .unwrap();
    t.execute(json!({"operation": "add", "item": "A-2", "__session_id": "local:tui::local_user"}))
        .await
        .unwrap();
    t.execute(json!({"operation": "add", "item": "B-1", "__session_id": "qq:group:1:2"}))
        .await
        .unwrap();
    let a = t
        .execute(json!({"operation": "list", "name": "default", "__session_id": "local:tui::local_user"}))
        .await
        .unwrap();
    assert!(a.contains("A-1") && a.contains("A-2"), "got: {a}");
    assert!(!a.contains("B-1"), "B 会话条目泄漏到 A: {a}");
    let b = t
        .execute(json!({"operation": "list", "name": "default", "__session_id": "qq:group:1:2"}))
        .await
        .unwrap();
    assert!(b.contains("B-1"), "got: {b}");
    assert!(!b.contains("A-1"), "A 会话条目泄漏到 B: {b}");

    let sa = t.snapshot_for_session("local:tui::local_user").unwrap();
    let sa_text = sa.to_string();
    assert!(
        sa_text.contains("A-1") && !sa_text.contains("B-1"),
        "snapshot: {sa_text}"
    );
    let sb = t.snapshot_for_session("qq:group:1:2").unwrap();
    let sb_text = sb.to_string();
    assert!(
        sb_text.contains("B-1") && !sb_text.contains("A-1"),
        "snapshot: {sb_text}"
    );

    // 权威空：未知会话也返回 Some（空 lists + 当前 epoch/revision）。
    let unknown = t.snapshot_for_session("unknown:session").unwrap();
    assert_eq!(unknown["lists"], json!([]));
}

/// 缺失 `__session_id`（联邦远程调用/直接 registry 执行）→ 全局兜底
/// 键，不与本地会话互相污染。
#[tokio::test]
async fn missing_session_falls_back_to_global_key() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation": "add", "item": "G-1"}))
        .await
        .unwrap();
    let local: Value = serde_json::from_str(
        &t.execute(
            json!({"operation": "get", "name": "default", "__session_id": "local:tui::local_user"}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        local["exists"],
        json!(false),
        "本地会话不应看到全局兜底清单"
    );
    let global = t
        .execute(json!({"operation": "get", "name": "default"}))
        .await
        .unwrap();
    assert!(global.contains("G-1"), "got: {global}");
}

#[tokio::test]
async fn read_and_failed_calls_leave_no_state() {
    let t = ChecklistTool::new();
    let before = t.state_snapshot().unwrap();
    for args in [
        json!({"operation":"list"}),
        json!({"operation":"reset"}),
        json!({"operation":"check", "index":1}),
        json!({"operation":"remove", "index":0}),
        json!({"operation":"add"}),
        json!({"operation":"add", "item":"  "}),
        json!({"operation":"bogus"}),
    ] {
        let _ = t.execute(args).await;
        assert_eq!(t.state_snapshot().unwrap(), before);
        let empty = t.snapshot_of(FALLBACK_SESSION).unwrap();
        assert_eq!(empty["lists"], json!([]));
        assert_eq!(empty["revision"].as_u64(), Some(0));
    }
    t.execute(json!({"operation":"add", "item":"keep"}))
        .await
        .unwrap();
    let before = t.state_snapshot().unwrap();
    let rev = revision(&t, FALLBACK_SESSION);
    assert!(t
        .execute(json!({"operation":"check", "name":"absent", "index":2,
                        "expected_epoch": epoch(&t, FALLBACK_SESSION),
                        "expected_revision":rev}))
        .await
        .is_err());
    assert_eq!(t.state_snapshot().unwrap(), before);
}

#[tokio::test]
async fn created_empty_list_is_visible_in_snapshot() {
    let t = ChecklistTool::new();
    t.execute(json!({"operation":"create", "name":"empty"}))
        .await
        .unwrap();
    let state = t.snapshot_of(FALLBACK_SESSION).unwrap();
    assert_eq!(state["lists"][0]["name"], "empty");
    assert_eq!(state["lists"][0]["items"], json!([]));
    let epoch = state["epoch"].clone();
    let rev = state["revision"].as_u64().unwrap();
    t.execute(json!({"operation":"reset", "name":"empty"}))
        .await
        .unwrap();
    let state = t.snapshot_of(FALLBACK_SESSION).unwrap();
    assert_eq!(state["lists"], json!([]));
    assert_eq!(state["epoch"], epoch);
    assert_eq!(state["revision"].as_u64(), Some(rev + 1));
}

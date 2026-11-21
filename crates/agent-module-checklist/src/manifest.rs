//! 模块身份（**单一真值**：`module.toml`）。
//!
//! 代码用 `include_str!` 读 `module.toml` 构造 manifest——改标题 / 描述 / id 都
//! 不必改 Rust，且 `serve.rs` 的 `manifest_claim()` 与任何未来的宿主形态读的是
//! 同一份，不会出现"面板显示一个名字、协议上报另一个"。
//!
//! 2026-11-21 起本模块**只有进程外形态**：Core 不依赖这个 crate，`module.toml`
//! 里不再有 `requires` / `provides`（那些是进程内 `Ctx` 服务表的接缝，进程外
//! 形态没有服务表可查）。保留的只有身份与人类可读描述。

use echo_module::ModuleManifest;

/// 模块声明文件（身份 + 描述）。
pub const MANIFEST_TOML: &str = include_str!("../module.toml");

/// 模块清单：从 [`MANIFEST_TOML`] 解析，只把版本号留给宿主注入。
pub fn manifest(version: &str) -> ModuleManifest {
    ModuleManifest::from_toml(MANIFEST_TOML, version)
        .expect("agent-module-checklist 的 module.toml 解析失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MODULE_ID;

    /// `module.toml` 里的身份必须与 Rust 常量同源：两边漂移会让面板显示
    /// 一个 id、服务键用另一个 id。
    #[test]
    fn manifest_matches_module_id() {
        let manifest = manifest("0.0.0-test");
        assert_eq!(manifest.id, MODULE_ID);
        assert_eq!(manifest.kind, "tool");
    }
}

# P1 — 记忆可写 + 常驻注入(memory_write 工具与精选记忆摘要)

> 本计划自包含,执行者无需其他上下文。所有代码标识符/路径均已核实存在。
> 执行原则:**镜像现有实现,不发明新模式**。每个任务都指定了要镜像的范本文件。

## 背景与目标

Doggy 的记忆系统(`xai-grok-memory`)目前只读:agent 有 `memory_search` / `memory_get`,
但**没有写入工具**——写入只能靠会话日志 + dream 批量整合,反射弧太长。
且精选记忆(MEMORY.md)不进 system prompt,模型"不知道自己不知道"。

P1 交付三件事:

1. **T1**: 新增 `memory_write` 工具(add / replace / remove,子串匹配定位条目)
2. **T2**: 精选记忆硬容量上限 + 溢出报错(逼 agent 当场合并/淘汰)
3. **T3**: 会话开始时把精选记忆摘要注入 system prompt(冻结快照,硬预算)

## 非目标(不要做)

- 不做后台自省闭环(P2)
- 不做 USER.md 用户档案拆分、写入审批 UX、完整注入攻击扫描(P3)
- 不改 dream 整合逻辑;`MemoryStorage::write_long_term` 的行为与签名**保持不变**
- 不修 `xai-grok-shell` 的 `--lib` 测试目标编译问题(已知的既有问题,与本任务无关)
- 不重命名任何现有工具/常量(有 pin 测试)

## 现有代码地图(已核实)

| 位置 | 内容 |
|---|---|
| `crates/codegen/xai-grok-tools/src/implementations/memory/` | `search_tool.rs`、`get_tool.rs`、`types.rs`、`mod.rs`(工具名常量 + pin 测试) |
| `crates/codegen/xai-grok-tools/src/registry/types.rs` ~L707 | `b.register::<...MemorySearchImpl>();` 注册点 |
| `crates/codegen/xai-grok-tools/src/tool_taxonomy.rs` L56-57, L85-86 | `ToolKind::MemorySearch/MemoryGet` 显示名与分组 |
| `crates/codegen/xai-grok-memory/src/storage.rs` | `MemoryStorage`:`global_memory_file()`、`write_long_term(scope, content)`、`append_to_memory(...)`、ephemeral 跳过逻辑、`MemoryScope::{Global,Workspace}` |
| `crates/codegen/xai-grok-memory/src/watcher.rs` | 文件监视(写 MEMORY.md 后索引刷新可能已自动;T1 步骤 6 验证) |
| `crates/codegen/xai-grok-agent/src/prompt/context.rs` | `PromptContext`,已有 `memory_enabled` / `memory_global_path` 占位符与测试范式(如 `test_placeholders_memory_enabled`) |
| `crates/codegen/xai-grok-agent/src/builder.rs` L326 | `with_memory_enabled(bool)`;grep `with_memory_enabled(` 找 shell 侧调用点 |
| `crates/codegen/xai-grok-agent/templates/prompt.md`、`subagent_prompt.md` | minijinja 模板,`<memory>` 条件段已存在于 subagent 模板 |
| `crates/codegen/xai-grok-agent/scripts/encrypt_templates.py` | **模板改动后必须重跑**,否则守卫测试报 stale |

## T1 — `memory_write` 工具

范本:通读 `get_tool.rs` 与 `search_tool.rs` 后再动手,镜像其 Tool trait 实现、
storage/backend 获取方式、错误处理与测试风格。

1. 新建 `crates/codegen/xai-grok-tools/src/implementations/memory/write_tool.rs`:
   - `MemoryWriteInput`(加进 `types.rs`,镜像现有 schemars 风格):
     - `action: String` — `"add" | "replace" | "remove"`
     - `scope: Option<String>` — `"workspace"`(默认)| `"global"`
     - `content: Option<String>` — add/replace 必填
     - `old_text: Option<String>` — replace/remove 必填,**唯一子串**定位条目
   - 条目模型:MEMORY.md 按**空行分隔的块**视为条目(兼容现有自由格式)。
   - 语义:
     - `add`:追加条目。与现有条目完全相同 → 成功返回 "duplicate, not added",不写盘
     - `replace`:`old_text` 唯一命中一条 → 整条替换为 `content`
     - `remove`:`old_text` 唯一命中一条 → 删除
     - 命中 0 条或 ≥2 条 → 报错,提示提供更特异的子串,并在错误里列出当前条目
   - 入口安全检查(轻量版):`content` 含不可见 Unicode(零宽/双向控制符)或
     `<system-reminder` 字样 → 拒绝写入
2. 容量检查见 T2;溢出时返回 T2 规定的结构化错误。
3. `mod.rs`:加 `pub const MEMORY_WRITE_TOOL_NAME: &str = "memory_write";` 与
   `pub use write_tool::MemoryWriteImpl;`,并在现有 pin 测试
   `memory_tool_constants_match_registered_ids` 中补 write 断言(镜像现有两条)。
4. `registry/types.rs` L707 旁注册 `MemoryWriteImpl`。
5. `tool_taxonomy.rs`:找到 `ToolKind::MemorySearch` 的**定义处**(grep 定义 site),
   加 `MemoryWrite` 变体;显示名 `"Memory Write"`;分组与 MemorySearch/MemoryGet 一致
   (L85-86 的分组 match 也要加)。
6. 写盘后索引刷新:先验证 `watcher.rs` 是否监视精选 MEMORY.md(写后查
   `chunks_without_embeddings` 或看 watcher 注册路径)。若不覆盖,在工具成功路径里
   镜像 dream/flush 的 reindex 调用;若覆盖,注释说明依赖 watcher 即可。

## T2 — 精选记忆容量上限

1. 配置:在 `crates/codegen/xai-grok-config-types/src/memory.rs` 加
   `curated_char_limit: u64`,默认 `2200`(全局与工作区各自独立计算)。
   镜像该文件现有字段的 serde/default 风格。
2. 上限只作用于 `memory_write` 工具路径(add/replace 后的**整文件字符数**);
   `write_long_term`(dream 用)**不设限、不改动**。
3. 溢出错误必须结构化返回(镜像下述文案,保留字段名):

   ```json
   {
     "success": false,
     "error": "Memory at {used}/{limit} chars. This {action} ({n} chars) would exceed the limit. Consolidate now: use 'replace' to merge overlapping entries into shorter ones, or 'remove' stale entries (see current_entries), then retry — all in this turn.",
     "current_entries": ["..."],
     "usage": "{used}/{limit}"
   }
   ```

   (文案改编自 hermes-agent,MIT 许可;见仓库 `D:\hermes-agent\LICENSE`)

## T3 — 精选记忆摘要注入 system prompt

1. `PromptContext`(context.rs)加字段 `memory_digest: Option<String>`,
   进占位符 map(镜像 `memory_enabled` 的接线与测试写法)。
2. 组装时机:找 `with_memory_enabled(` 的 shell 侧调用点,在同一处读
   全局 + 工作区 `MEMORY.md`,组装摘要:
   - 预算:**3200 字符**(约 800 token),工作区条目优先、全局其次,超预算按条目截断
     (不截半条);两文件都空 → `None`
   - 头部格式(单行):`MEMORY [{pct}% — {used}/{limit} chars]`,后接条目原文
   - **冻结快照**:会话开始读一次,会话中不更新(保护 prefix cache)
3. 模板:`prompt.md` 加 `<memory>` 条件段(镜像 `subagent_prompt.md` 的
   `${%- if memory_enabled ... %}` 写法),`memory_digest` 存在时渲染;
   subagent 模板的 `<memory>` 段同步加 digest 渲染。
4. **模板改动后必须执行**:
   `cd crates/codegen/xai-grok-agent && python scripts/encrypt_templates.py`
5. 测试:镜像 `test_placeholders_memory_enabled` 补 digest 的
   有/无/超预算三个用例。

## 验收标准(逐条可独立验证)

1. `memory_write` 以 id `"memory_write"` 注册,add/replace/remove 语义、
   子串歧义报错、重复 no-op 均有单测覆盖并通过
2. 溢出时返回上述结构化错误(含 `current_entries` 与 `usage`),文件未被修改
3. `memory_enabled=true` 且 MEMORY.md 非空时,渲染后的 system prompt 含摘要块且
   ≤ 预算;`memory_enabled=false` 或文件空时不含
4. `cargo test -p xai-grok-tools --lib memory` 全绿
5. `cargo test -p xai-grok-agent --lib prompt` 中 `prompt::template` 全绿
   (加密副本已重新生成;`prompt::skills` 有 2 个**既有**失败,与本任务无关,不要修)
6. `cargo check -p xai-grok-shell --lib` 通过
7. `git diff` 不含对 `write_long_term` 行为、现有工具名常量、dream 逻辑的改动

## 陷阱清单(执行前通读)

- `prompt_encrypted.rs` 是生成物,**手改必坏**,只能跑脚本重生成
- `xai-grok-shell` 的 `cargo test --lib` 因缺失测试模块**本来就编译不过**——
  用 `cargo check` 验证该 crate,不要试图修它
- 本机是 Windows:路径拼接一律走现有 `grok_home()` / `PathBuf`,不要硬编码 `/`
- ephemeral workspace(临时目录 CWD)下工作区写入会被 storage 静默跳过——
  工具对此返回明确提示(镜像 storage 的 `MEMORY_EPHEMERAL_SKIP` 处理)
- 新增 prompt 文本(工具描述等)风格与现有工具一致:简洁、面向行为、无营销语
- 每个新常量/契约补 pin 测试(参考 `mod.rs` 的 `memory_tool_constants_match_registered_ids`)

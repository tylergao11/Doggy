# P2 — 后台自省闭环(Reflection Loop)

状态:已实现(T0 → T1 → T2)。前置:P1 已交付并终审通过(memory_write / 容量闸 / 冻结 digest)。

## 目标

会话结束时,后台用一次模型调用回顾本会话,把值得跨会话记住的东西以 add/replace/remove
操作写进精选 MEMORY.md(走 P1 同一套容量闸与安全检查),让 Doggy 越用越聪明。

## 侦察发现的架构冲突(T0 的动机)

Dream 整合把 ≤16,000 字符的 `## 标题` 文档整体写进 workspace `MEMORY.md`
(`execute_dream` → `write_long_term(Workspace)`),而 P1 把同一个文件定义为
≤2,200 字符的精选条目库,digest 预算 3,200。一次 dream 就能击穿精选容量,
此后所有 `memory_write` add/replace 永远溢出。**双写者必须分家。**

## 任务分解

### T0 — Dream 输出分流到 `consolidated.md`

- `MemoryStorage`(xai-grok-memory/storage.rs):
  - 新增 `workspace_consolidated_file()` → `<workspace_dir>/consolidated.md`
  - 新增 `write_consolidated(&self, content)`(镜像 `write_long_term` 的原子写;ephemeral 跳过)
  - 新增 `migrate_dream_output_if_needed(curated_limit) -> bool`:
    `consolidated.md` 不存在 且 workspace `MEMORY.md` 非脚手架 且 长度 > curated_limit
    → 内容整体搬到 `consolidated.md`,`MEMORY.md` 清空。一次性、幂等。
- `dream.rs`:`execute_dream` 写目标改为 `consolidated.md`;
  `run_dream_inner`(shell)读取的 existing_memory 改为 `consolidated.md`;
  dream 后 reindex 的路径同步改。既有 dream 测试同步更新断言。
- 迁移调用点:session spawn 组装 digest 之前(spawn.rs P1 改动处附近)。
  索引刷新依赖 watcher(与 memory_write 同一约定)。
- `classify_source`:确认 `consolidated.md` 归类(预期落在 workspace 类,维持 evergreen 不衰减)。

### T1 — 反思核心(纯逻辑,xai-grok-memory/src/reflection.rs)

- `REFLECTION_SYSTEM_PROMPT`:角色 = 精选记忆馆长。只收跨会话耐久事实
  (用户纠正过的偏好、项目不变量、环境硬知识);不收任务进度;条目 ≤240 字符;
  接近容量优先 replace 合并;无可记则输出 `[]`。
  输出契约:仅一个 JSON 数组
  `[{"action":"add|replace|remove","scope":"workspace|global","content":?,"old_text":?}]`。
- `build_curated_context(ws_md, global_md, limit) -> String`:当前条目 + 用量注入最后的 user 消息。
- `parse_reflection_ops(response, max_ops) -> Result<Vec<ReflectionOp>, String>`:
  提取首个平衡 JSON 数组,校验 action/scope,截断到 max_ops。
- `apply_reflection_ops(ops, ws_md, global_md, limit, ws_writable) -> ReflectionApplyReport`:
  逐条走 xai-grok-tools 的 `apply_action` + 容量闸 + `content_safety_error`
  (这些原语从 pub(crate) 提升为 pub);溢出/不安全/定位失败的 op 记入 skipped,不重试。
- 依赖方向成立:xai-grok-memory 已依赖 xai-grok-tools(实现 MemoryBackend)。

### T2 — Shell 接线(触发、门控、通知)

- 配置 `[memory.reflection]`(xai-grok-config-types):
  `enabled: true`(随 --experimental-memory 伞,与 dream/flush 默认一致)、
  `model: Option<String>`(None → flush_model → 会话主模型)、
  `max_ops: 4`、`min_real_user_messages: 3`、`timeout_secs: 120`、
  `apply: "auto" | "staged"`(默认 auto)。
- `SessionMemory` 增加 `reflection_config` 字段。
- `memory_dream.rs` 新增 `maybe_run_reflection()`:
  - 门控:memory 启用、reflection 启用、非 subagent、真实用户消息 ≥ 阈值、每会话最多 1 次。
  - 输入:最近对话窗口(复用 `select_flush_window`,20 条)作为 items,
    最后追加 curated 上下文 user 消息;模型调用镜像 dream(超时 120s)。
  - 应用:`apply_reflection_ops` → `write_long_term` 写回两个 scope;
    ephemeral workspace 只跳过 workspace scope 的 op。
  - staged 模式:操作以 JSON 行追加到 `<workspace_dir>/reflection_pending.jsonl`,不落 MEMORY.md
    (审批 UX 属 P3)。
  - 通知:`XaiSessionUpdate::MemoryReflectionCompleted { result, path }`(镜像 Dream 变体)。
- 触发点:run_loop.rs 两处 session-end 路径,`maybe_run_dream()` 之前调用
  (反思先写精选,dream 再整合旧日志,顺序无竞争)。

### Digest 回放策略(设计决定,不写代码)

系统提示中的 digest 冻结整个会话(保护 prefix cache):
memory_write / 反思写盘只改磁盘 + 通知,新 digest 下个会话生效。与 Hermes 相同。

## 非目标

- 不做每轮(per-turn)反思——成本失衡;flush 已覆盖会话内情景记录。
- 不做审批 UX、`/memory pending`、USER.md 拆分、注入攻击全量扫描(P3)。
- 不改 `write_long_term` 签名;不改 MEMORY_SEARCH/GET/WRITE 常量值。
- 不改 flush 逻辑与触发。

## 验收标准

1. `cargo test -p xai-grok-memory` 全绿(reflection 单测 + 更新后的 dream/storage 测试)。
2. `cargo test -p xai-grok-tools --lib memory` 全绿(原语提升 pub 不破坏既有测试)。
3. `cargo check -p xai-grok-shell --lib` 通过。
4. dream 完成后 workspace `MEMORY.md` 不再被 dream 触碰;`consolidated.md` 承接 dream 输出。
5. 迁移:大于容量的旧 dream 式 MEMORY.md 一次性搬入 consolidated.md,幂等。
6. 反思:解析容错(非 JSON → 记 failed 不 panic)、max_ops 截断、容量溢出 op 跳过且文件不写坏、
   ephemeral 只跳 workspace op——均有单测。
7. `cargo test -p xai-grok-agent --lib prompt::template` 全绿(本阶段不动模板)。

## 实现过程中追加的改动(计划外但必需)

- `list_memory_files()` 与 `build_memory_archive()` 补入 `consolidated.md`。二者分别喂
  会话启动时的索引重建和 `/memory` 浏览 / 记忆导出;不补的话 dream 输出只能靠 watcher
  机会性入索引,且在 UI 和导出里彻底消失。
- `MEMORY_WRITE_TOOL_NAME` 补进 `register_memory_tools`(P1 遗漏):中途 `/memory on`
  开启记忆时只注册了 search/get,memory_write 不可调用。
- `backend.rs` 的一处测试构造器补 `curated_char_limit`(P1 遗漏,导致 `xai-grok-memory`
  的 lib 测试目标编译不过)。
- workspace 脚手架文案从 "Auto-populated by dream consolidation" 改为
  "Curated project notes",并把新文案加入 `is_scaffold_template` 的标记表(旧标记保留,
  以便老版本写下的脚手架仍能正确识别)。
- `parse_entries` 过滤模板脚手架(纯标题行、HTML 注释、生成的免责声明)。P1 遗留:
  自动生成的 MEMORY.md 模板会被当成真实条目——每个新工作区都往 system prompt 注入
  模板文字,并让容量统计从非零起步。在唯一解析入口修,digest / 容量闸 / 反思三处口径一致。
- dream 与反思写盘后的增量索引改用 `classify_source(path)`,与会话启动时的全量重建
  一致。此前 dream 传字面量 `"dream"`,该来源既不在 evergreen 名单里(会被时间衰减),
  也不在 evergreen 补充检索的 source 过滤里,同一文件的 source 会随索引路径而变。

# P3 — 治理与打磨(USER 档案 / 注入面 / 审批 UX)

状态:T1 / T2 / T3 / T4 均已完成。前置:P1(memory_write + 容量闸 + 冻结 digest)、
P2(dream/curated 分家 + 会话结束反思)。

## 目标

把"能写记忆"变成"可托付":用户档案与项目知识分层、写入内容不能撬动 system prompt 结构、
暂存的反思操作有地方审。

## 任务分解

### T1 — USER.md 用户档案分层(已完成)

今天全局 `MEMORY.md` 同时装"用户是谁"和"跨项目的技术事实",前者稳定、后者流动,混在一起
会互相挤占容量。拆出第三个 scope:

| scope | 文件 | 内容 |
|---|---|---|
| `user` | `~/.Doggy/memory/USER.md` | 用户偏好、工作方式、称呼、语言 |
| `global` | `~/.Doggy/memory/MEMORY.md` | 跨项目技术事实 |
| `workspace` | `<ws>/MEMORY.md` | 本项目不变量 |

- `MemoryScope::User` + `MemoryStorage::user_memory_file()`;`ensure_initialized` 建脚手架。
- `classify_source`:USER.md → `"global"`。必须落在 evergreen 名单里,否则会被时间衰减,
  也进不了 evergreen 补充检索。
- `list_memory_files` / `build_memory_archive` 收录。
- `memory_write` 接受 `scope: "user"`;`MemoryBackend` 的 curated 读写支持该 scope。
- digest 顺序:user → workspace → global(最稳定的排最前,预算不足时先掉 global)。
- 反思提示词说明三层分工;`parse_reflection_ops` 接受 `user`;`apply_reflection_ops` 三路。

### T2 — 收窄注入面(已完成)

digest 以纯文本嵌在 system prompt 的 `<memory>...</memory>` 块里,所以真正的风险不是含糊的
"越狱措辞",而是**结构性突破**:一条记忆里出现 `</memory>`,后面的内容就变成顶层系统指令。

- `content_safety_error` 增加结构标签拒绝:`</memory>`、`<user_query`、`<user_info`、
  `<git_status`、`<environment_details`(`<system-reminder` 已有)。
- 单条上限 `MAX_ENTRY_CHARS = 500`:防止一条把整个预算吃光,也压掉"长篇指令"这一类载荷。
- 反思路径自动继承(它复用 `content_safety_error` 与 `apply_action`)。

实现时补上一处计划外的缺口:写入闸只管**经过工具**的内容,而 `MEMORY.md` 是明确邀请用户手改的
纯文本文件,老版本文件更是在这些规则之前就写好了。所以把检查拆成两层——`injection_hazard`
(隐藏字符 + 结构标签)在 **digest 组装时**再查一遍,越界条目不进 prompt 但留在文件里;
长度上限只在写入时管,已在盘上的长条目照旧注入(静默丢掉用户在文件里看得见的事实更糟)。

### T3 — 记忆审批(已完成)

P2 的 `apply = "staged"` 会把操作写进 `reflection_pending.jsonl` 但不落盘,目前没有出口。

**约束(用户指定):审批不得占用主流程。**这是全自动工程,反思不能因为等人点头而卡住,
所以审批必须是"拉取式"的:顶栏被动提示,用户想处理时自己去处理。默认仍是 `auto` 直接写入,
暂存队列只在配置为 `staged` 时才会有东西。

据此排除了两种做法:现成的 `ActiveModal` 会变暗全屏并接管快捷键栏;`ephemeral tip` 有 TTL,
几秒后消失——而反思是在**会话结束**时暂存的,那一刻用户最不可能在看屏幕。

落地形态:

- `xai-grok-memory::pending` — 追加式 JSONL 队列。容忍截断尾行与字段缺失(被 kill 时最多
  损失一条,不能让整个队列读不出来)。`approve_all` 复用 `apply_reflection_ops`,先写 curated
  文件再清队列;被拒的操作只报告不回队(否则徽标会永久卡住且用户无法清除)。
- `XaiSessionUpdate::MemoryPendingApprovals { count, path }` — 会话启动时与暂存后各发一次。
  没有队列文件就不读盘,也就是 `auto` 模式下零开销。
- 顶栏徽标 `◆ memory: N`。**不可点击、不绑按键**——认出队列存在就是它的全部职责。
- `/memory [pending|approve|discard]`:裸命令只列出,两个破坏性选择必须显式打出来。
  文件 I/O 走 `Effect` → `spawn_blocking`,不占 UI 线程。

### T4 — 启动路径工作量有界(已完成)

digest 组装在每次会话启动时读整个 MEMORY.md。容量闸只管 `memory_write`,手工编辑或外部工具
写入的超大文件不受约束。给 digest 的读取加字节上限,避免启动被拖慢。

`MemoryStorage::read_curated_prefix(path, max_bytes)` + `CURATED_DIGEST_READ_LIMIT`
(= digest 预算的 10 倍,32000 字节)。截断时退回到最后一个空行边界,免得 prompt 里出现半条
事实;落在多字节字符中间时只保留合法前缀,不产生替换字符。仅用于 digest —— 读改写路径绝不能
用它,丢掉的尾部会被当成删除写回去。

## 非目标

- 不做记忆内容的语义级越狱检测(误伤率高于收益)。
- 不改 dream / flush 的触发与逻辑。
- 不改 `write_long_term` 签名。

## 验收标准

1. `cargo test -p xai-grok-memory --lib` 全绿。
2. `cargo test -p xai-grok-tools --lib memory` 全绿。
3. `cargo test -p xai-grok-config-types --lib` 全绿。
4. `cargo test -p xai-grok-agent --lib prompt::template` 全绿(含加密模板不过期检查)。
5. `cargo check -p xai-grok-shell --lib` 与 `-p xai-grok-pager --lib` 通过。
6. USER.md 参与 digest、检索、列表、归档,且分类为 evergreen——均有单测。
7. `</memory>` 与超长条目被拒且文件不改——有单测。
8. `/memory` 三个子命令的解析有单测;队列的追加/读取/批准/丢弃有单测。

## T3 交付记录

- `crates/codegen/xai-grok-memory/src/pending.rs`(新增,16 项单测)
- `xai-grok-shell`:`notification.rs` 新增通知变体;`memory_dream.rs` 改用 pending 模块并新增
  `report_pending_approvals_at_start`;`run_loop.rs` 在 `DispatchSessionStartHook` 处 spawn 上报。
- `xai-grok-pager`:`memory_pending_count` 状态 + 顶栏徽标;`slash/commands/memory.rs`;
  `Action`/`Effect`/`TaskResult` 各两个变体;`dispatch/notes.rs` 四个处理函数。

验证:`cargo test -p xai-grok-memory --lib` 327 passed;`cargo check -p xai-grok-shell --lib`
与 `-p xai-grok-pager --lib` 均通过。

## T1 / T2 / T4 交付记录

- `xai-grok-memory/storage.rs`:`MemoryScope::User` + `parse`/`as_str`;`user_memory_file`;
  `clear_user`;`read_curated_prefix` + `CURATED_DIGEST_READ_LIMIT`;写入/追加/列表/脚手架三路。
- `xai-grok-memory/reflection.rs`:三层 body + `user_body`;未知 scope 显式拒绝(原本会静默
  落进 global);`build_curated_context` 三段;提示词按"生命周期而非主题"选 scope。
- `xai-grok-memory/archive.rs`:`global/USER.md` 入包。
- `xai-grok-memory/pending.rs`:`approve_all` 三路写回。
- `xai-grok-tools/…/memory/write_tool.rs`:`injection_hazard` / `MAX_ENTRY_CHARS`;
  `assemble_memory_digest` 改四参并在装配时过滤越界条目;USER.md 脚手架短语入过滤名单。
- `xai-grok-shell`:`spawn.rs` digest 三路有界读取;`memory_dream.rs` 反思读写三路。
- `xai-grok-pager`:`memory_cmd.rs` 新增 `--user`(`--all` 含三层);`memory_modal.rs`
  按文件名把 USER.md 拆成独立 **User** 分组(纯展示,不动 `classify_source` 的检索权重)。
- `templates/prompt.md`:文档路径 `~/.grok/` → `~/.Doggy/`(文档实际落在 `~/.Doggy/docs/
  user-guide/`,旧路径会让模型读空),已重跑 `encrypt_templates.py`。
- `docs/user-guide/13-memory.md`:三层存储表、`--user`、自我反思与暂存审批小节。

### 修掉的一处真 bug

`parse_reflection_ops` 的 scope 白名单还是 `workspace | global`。T1 让提示词开始教模型用
`user`,但解析器会把这些操作**静默丢掉**——档案永远写不进去。改为走 `MemoryScope::parse`,
并加了一条"三个 scope 都被接受"的回归测试。

### 验证

| 命令 | 结果 |
|---|---|
| `cargo test -p xai-grok-memory --lib` | 347 passed |
| `cargo test -p xai-grok-tools --lib memory` | 57 passed |
| `cargo test -p xai-grok-agent --lib prompt::template` | 39 passed(含加密模板不过期检查) |
| `cargo check -p xai-grok-shell --lib` / `-p xai-grok-pager --lib` | 通过 |

### 未能执行的验证

`xai-grok-pager` 的 `--lib` 测试目标无法编译,与本次改动无关:`app/acp_handler/tests.rs`
与 `app/dispatch/tests.rs` 两个模块文件在仓库里从未存在(`git ls-tree HEAD` 确认),而
`interject.rs` / `queue.rs` 等多处 `#[cfg(test)]` 都 import 其中的共享辅助
(`test_app_with_agent`、`end_turn`、`enqueue_local`);`interject.rs:309` 的 E0282 是这条
未解析导入的级联。因此 pager 侧改动只有 `cargo check` 级别的保证,`memory_modal` 与
`slash/commands/memory.rs` 的单测要等这个共享测试模块补回来才能跑。

`xai-grok-shell` 的 `--lib` 测试目标同样缺件,情况相同。

## 后续项(不在 P3 范围内)

- `docs/user-guide/` 下还有 121 处 `~/.grok` 旧路径,散在 20 个文件里(本次只改对了
  `13-memory.md`)。这是改名遗留的机械替换,单开任务处理;替换前需要区分"当前路径"与
  "迁移说明里故意提到的旧路径"。
- pager / shell 的 `--lib` 测试目标需要补回缺失的共享测试辅助模块,由用户处理。

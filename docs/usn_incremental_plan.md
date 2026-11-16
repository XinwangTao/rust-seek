# USN 增量刷新：实现记录

> 状态：**已实现（v5.4.0）**。设计定稿见 `docs/archify/usn_incremental_workflow.html`。
> 背景数据：本机 D 盘 31.7 万目录，全盘巡约 17–22 秒（过滤驱动下每个 stat ~1ms）。

## 1. 要解决的因果

启动时不知道「关软件期间改了什么」，只能 stat 每个目录比对 mtime。
USN 直接记录这段时间发生的每一次改名/新建/删除，读它即可得到「哪些目录脏了」。

## 2. 实现结构

| 文件 | 职责 |
|---|---|
| `src/usn.rs` | 开卷、查日志、读记录、FRN→路径、快照状态读写 |
| `src/index.rs` | `refresh_with_mode` 在全盘巡之前先试 USN；零命中可信判定 |
| `src/integration.rs` | 开机自启改为「登录时触发、最高权限」的计划任务 |

新增 `%LOCALAPPDATA%\rust-seek\usn-state.json`：

```json
{ "D:": { "journal_id": 134384185511282765, "next_usn": 965216816 } }
```

- `next_usn`：这份缓存已经覆盖到日志的哪一条。
- `journal_id`：日志被重建（ID 变了）时必须放弃增量。
- **不变量**：`next_usn` 只能在这份索引内容对应的缓存成功落盘之后才推进；
  写盘失败就不推进，否则别的进程会「旧缓存 + 新 next_usn」把这批变更永久跳过。

## 3. 接口

```rust
usn::can_attempt(&drives)               // 快查：值不值得同步试一次
usn::dirty_dirs_since_snapshot(&drives) // Some((脏目录, 待提交快照)) / None=回退
usn::snapshot_before_sweep(&drives)     // 全盘巡开始前记位置
usn::commit(&commits) / invalidate(&drives)
usn::needs_seed(&drives) / available(&drives)
```

## 4. 失败回退（逐条已实现）

1. 不是管理员 / 开卷失败 → `None` → 全盘巡。
2. `journal_id` 与持久化不一致 → `None`。
3. `StartUsn < LowestValidUsn`（日志被截断）→ `None`。
4. 单条 FRN→路径解析失败 → 整块盘 `None`，不赌增量。
5. 任何 USN 调用出错 → `None`，且本次不推进 `next_usn`。
6. 变化量超过 200 万条 → `None`（全盘巡更划算）。

## 5. 为什么不在索引条目里存 FRN

存 FRN 可以让增量直接定位条目，但会让缓存格式升版、FRN 在移动后变化、维护成本高。
第一版只把 USN 当作「哪些目录脏了」的来源，后续目录重枚举沿用现有
`refresh_in_place(hint)` 路径 —— 语义与监听推来的脏目录完全一致。

## 6. 接入点

`refresh_with_mode(index, allow_full_sweep)`：

```
if !NEED_FULL_SCAN { 应用 DIRTY_DIRS → refresh_in_place(Some(dirty)) }
else if let Some((dirty, commits)) = usn::dirty_dirs_since_snapshot() {
    refresh_in_place(Some(dirty));
    if dirty 非空 { 必须 save_cache 成功才 commit }
    USN_TRUSTED = true; NEED_FULL_SCAN = false;
}
else if allow_full_sweep {
    let sweep = usn::snapshot_before_sweep();
    refresh_in_place(None);        // 原全盘巡
    save_cache(); usn::commit(sweep);
}
```

`ensure_ready` 额外做两件事：

1. 需要全盘巡时，先 `try_load()` 再同步试一次 USN 增量（毫秒级），成功就不用甩后台；
2. 有管理员权限但还没有增量起点时，后台建一次索引把快照「种」下来（跨进程节流）。

## 7. 验证结果（2026-11-16 本机实测）

| 场景 | 改前 | 改后 |
|---|---|---|
| 热查询（同进程） | 60 ms | 60 ms |
| 新进程首次查询 | 1.3–13.7 s | 1.5–1.8 s |
| 零命中（索引不可信） | 35 s | 1.7 s |
| 新建文件可见性 | 需全盘兜底 | USN 增量，1.5 s 命中 |

单元测试：`cargo test` 覆盖日志 ID 变化 / 起点过旧 / 记录解析 / 盘根归一化。
实机探针（`--ignored`）：`live_usn_roundtrip`、`live_seed_usn_after_build`、
`live_usn_sees_new_file`、`live_journal_probe`。

## 8. 提权

读卷需要管理员权限。改法：开机自启由 `HKCU\...\Run` 改为计划任务
（`schtasks /Create /SC ONLOGON /RL HIGHEST`），托盘可直接开关。
代价：托盘程序以管理员常驻；资源管理器拖拽到提权窗口会被 UIPI 拦住。
没有管理员权限时一切照旧（自动回退全盘巡），不会出错。

## 8.1 v5.4.1 追加（2026-11-16）

- **父 FRN 解析失败不再直接放弃**：USN 记录自带文件名，父目录已被删除（临时目录很常见）时，
  顺着「父 FRN + 名字」最多 8 层把路径拼回来。拼出来的目录已不存在，交给增量刷新正好
  把它整棵子树从索引里删掉。实测删掉一棵树后增量仍可用（`failed=0`）。
- **剪掉 NTFS 元数据目录**：`$Extend`（被删项会被挂到 `$Extend/$Deleted`）加入 `SKIP_DIRS`，
  避免枚举不了却把 `incomplete_dirs` 标脏。
- **缓存载入并行化**：UTF-8 按 ASCII 切点分段校验、条目/目录按块并行反序列化。
  790MB 缓存载入 425ms → ~390ms（内存拷贝本身仍是主要成本）。
- **诊断**：`RUST_SEEK_DEBUG=1` 会打印 USN 记录数/父目录数/解析失败数、各次 refresh 的
  脏目录数与阶段耗时，便于定位「为什么这次慢」。

## 8.2 v5.4.2 追加（2026-11-16）：实时输入不再刷新索引

实测：只要有文件变更（系统后台一直在写日志），**每次查询都要先同步刷新 240–500ms**。
GUI 边打字边搜，等于每个按键都在付这笔钱。

改法：实时输入（`record_history=false`）走 `ensure_fresh_quick()`——只用当前快照作答，
变更交给后台线程 `schedule_refresh()`（进程内 1 秒节流，且只用增量路径，绝不在后台起全盘巡）。
回车/点搜索仍走同步刷新，保证显式搜索的结果是最新的。

实测：`ensure_fresh_quick` 返回 119µs（原来最坏 500ms）。

## 8.3 v5.4.3：水位并入缓存（对抗性审查后的结构性修复）

**审查发现的两个真问题**：

1. **索引内容与水位不是原子更新**。原来索引写 `index-v1.bin`、水位写 `usn-state.json`，
   两次 rename 之间可被别的进程插入。推演出的危险交错：B 保存含变更的新缓存 →
   A 用旧内容覆盖缓存 → A 提交旧水位 → B 提交新水位，结果是「旧缓存 + 新水位」，
   那批变更被永久跳过且无任何报错。**单进程测试永远测不出来。**
2. **可移动盘导致重建循环**。`local_drives()` 含 U 盘，而 U 盘通常没有 USN 日志；
   `needs_seed` 只要「任一盘缺水位」就为真 → 插着 U 盘时每 10 分钟重建一次 760 万条索引，
   且永远种不上水位。

**修法（第一性原理）**：索引是「某时刻文件系统的快照」，水位就是这个快照的坐标，
二者是同一实体。所以：

- 缓存格式升 v5（`RSKIDX`）：头部多 `usn_count`，末尾追加每盘 24 字节水位；
  读写同一个文件、同一次 rename → 原子。
- 删除 `usn-state.json` 及其读写代码；`dirty_dirs_since_snapshot(drives, state)` 的
  `state` 直接来自内存索引（它就是从缓存读出来的）。
- 只有缓存真的写成功才推进水位；不写缓存的路径（增量 stat 巡）不推进，下次重放（幂等）。
- `needs_seed` / `can_attempt` 只认**有日志的盘**；存在无日志的盘时整机回退全盘巡，
  但不会再触发重建循环。
- v4 缓存仍可读（没有水位）→ 随后一次全盘巡把水位种上，避免升级即重扫 760 万条。

**验证**：`live_usn_from_cache` 断言水位能从缓存读出并直接续增量、且不再生成
`usn-state.json`；实测新进程查询 1.28s（USN 306 个脏目录、刷新 182ms）。

## 8.4 v5.4.4：单写者 + 只读快照共享索引

**要解决的两笔账**：每个进程各持一份 ~790MB 索引（GUI + 8 个 MCP ≈ 7GB），
以及每次载入要复制 + 校验 ~390ms。

**做法**（图：`docs/archify/shared_index_workflow.html`）：

1. **零拷贝快照**：缓存升 v6（`RSKIDX`），在 blob 后、目录文本后各补 0~7 字节，
   让条目/目录块 **8 字节对齐**；`Item`/`DirRec` 改 `#[repr(C)]`。载入时直接 mmap 成
   `&[Item]` / `&str`，只做一次并行 UTF-8 校验，不复制。
   `Index` 的 `payload` 是 `Owned | Mapped` 二选一；写路径调 `ensure_owned()` 做一次
   copy-on-write。v4/v5 缓存仍可读（走复制路径）。
2. **单写者**：GUI 启动时 `set_gui_mode()` 并每 10 秒写心跳文件 `gui-alive`；
   MCP 进程启动时若心跳新鲜 → **读者模式**：不监听、不刷新、不写盘，只按缓存文件的
   (长度, mtime) 变化重映射。
3. **写者定时落盘**：GUI 后台每 30 秒 `ensure_fresh()`，有变更才刷新并保存，
   保证读者看到的快照不会长期停在旧版本。

**实测**：缓存载入 390ms → **33ms**；MCP 新进程整机查询 1.2–1.5s → **132ms**；
读者不复制数据，映射页由系统页缓存跨进程共享。

**取舍**：读者最多比写者旧一个落盘周期（通常 ≤30 秒）；索引内容本来就不可变，
读者的零命中按「30 分钟内的快照可信」处理，结果标 `index_maybe_stale`。

## 8.5 v5.4.6：角色动态化 + 发布不持锁写盘

对抗性审查（Munger/Meadows/Feynman/Musk）发现三处结构性问题：

1. **读者角色被冻结**：`READER_DECIDED` 只判定一次。GUI 一关，已运行的 MCP 读者
   永远停在旧快照，也不会接管写者；缓存过期（7 天）后甚至每次查询都退化全盘遍历。
   → 改为**每次现判** `gui_alive()`：写者消失读者自动接管，写者回来再退回只读。
2. **发布时持锁写盘**：`persist_if_dirty` 拿着索引锁做完 790MB 序列化 + 1–3 秒写盘，
   写者自己的查询被堵住。→ 拆成 `build_cache_bytes()`（锁内，~300ms）与
   `write_cache_bytes()`（锁外，不阻塞查询）。
3. **布局没有编译期护栏**：`Item`/`DirRec` 与缓存字节布局强绑定，改字段却忘改
   `ITEM_BYTES`/`MAGIC` 会静默读错。→ 加 `const _: () = assert!(size_of::<Item>() == ITEM_BYTES)`。

实测：删掉心跳文件后新进程自动转写者并补 USN（123k 条记录 / 4173 个脏目录）。

## 8.6 v5.4.7：快照「可信」按写者判定，不按文件年龄

审查发现一个真实误判：写者**只在内容真变时才落盘**（为省 790MB 写入），
所以「缓存文件一小时没更新」既可能是「一小时没变化」，也可能是「没落盘」。
旧逻辑用快照年龄判定可信，实测有空闲 6 分钟后 `index_age_ms=352s` 的情况——
再往后读者就会把「没有变化」误判成「快照过期」，零命中退回 35 秒全盘遍历。

- **心跳语义修正**：心跳改由**维护循环**在每轮刷新+落盘之后写（原来由独立线程写）。
  于是「心跳在跳」＝「索引真的在被维护」，而不是「进程还活着」。维护线程卡住 → 心跳停跳。
- **可信判定改看写者**：`reader_mode()` 为真即说明写者活着在维护，零命中直接返回；
  不再依据 `index_age`。写者一停，`reader_mode()` 立刻为假，本进程转为写者自己刷新。

实测：写者空闲时快照年龄 24 秒仍保持 148ms 零命中响应（不再退化）。

## 9. 未做

- `FSCTL_ENUM_USN_DATA` 全量枚举：本机实测受过滤驱动限速 ~30MB/s（D 盘 850 万条 27 秒），
  比目录遍历还慢，因此不用它建索引。
- 索引条目里存 FRN（见 §5）。
- 多进程共享内存索引（缓存载入仍是 ~1.1 秒/进程）。

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

## 9. 未做

- `FSCTL_ENUM_USN_DATA` 全量枚举：本机实测受过滤驱动限速 ~30MB/s（D 盘 850 万条 27 秒），
  比目录遍历还慢，因此不用它建索引。
- 索引条目里存 FRN（见 §5）。
- 多进程共享内存索引（缓存载入仍是 ~1.1 秒/进程）。

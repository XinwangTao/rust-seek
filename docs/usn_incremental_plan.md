# USN 增量刷新：实现蓝图

> 状态：设计定稿，代码未写。目标：用 NTFS USN 变更日志替代「启动时全盘目录 stat 巡」。
> 背景数据：本机 D 盘 31.7 万目录，全盘巡约 17–22 秒（过滤驱动下每个 stat ~1ms）。
> 前置：D 盘日志已创建（ID `0x01dd6db468d4884d`，32MB，2026-11-06）。

## 1. 要解决的因果

启动时不知道「关软件期间改了什么」，只能 stat 每个目录比对 mtime。
USN 直接记录这段时间发生的每一次改名/新建/删除，读它即可得到「哪些目录脏了」。

## 2. 数据结构与持久化

新增 `%LOCALAPPDATA%\rust-seek\usn-state.json`：

```json
{
  "D:": { "journal_id": "0x01dd6db468d4884d", "next_usn": 24240, "dir_frn": 123456 },
  "C:": { "journal_id": "0x...", "next_usn": 0, "dir_frn": 0 }
}
```

- `next_usn`：上次成功处理到哪一条；全盘巡成功后立即写入。
- `journal_id`：日志被重建（ID 变了）时必须放弃增量。
- 索引缓存格式不变（v4），**不引入 dir_id/frn 到条目里**——见 §5 取舍。

## 3. 关键 API（windows-sys 已有 Win32_System_IO / Win32_Storage_FileSystem 特性）

| 步骤 | 调用 | 失败时 |
|---|---|---|
| 开卷 | `CreateFileW(r"\\.\D:", GENERIC_READ, FILE_SHARE_READ\|WRITE\|DELETE)` | 返回 None（无权限/非 NTFS） |
| 取日志信息 | `DeviceIoControl(FSCTL_QUERY_USN_JOURNAL)` | None |
| 读记录 | `DeviceIoControl(FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0{ StartUsn, ReasonMask, ReturnOnlyOnClose=0 })` | None |
| FRN → 路径 | `OpenFileById` + `GetFinalPathNameByHandleW` | 该条跳过并置 `partial=true` |

`ReasonMask = FILE_CREATE | FILE_DELETE | RENAME_OLD_NAME | RENAME_NEW_NAME | DATA_OVERWRITE | DATA_EXTEND | CLOSE`

## 4. 失败回退（必须逐条实现，缺一条就会静默丢文件）

1. 不是管理员 / 开卷失败 → **回退全盘巡**（当前行为）。
2. `journal_id` 与持久化不一致（日志被删重建）→ 回退全盘巡。
3. `StartUsn < LowestValidUsn`（日志被截断，中间丢了记录）→ 回退全盘巡。
4. 单条 FRN→路径失败（文件已删、权限）→ 记 `partial=true`；`partial` 时对整个受影响的盘**仍走全盘巡**，不赌。
5. 任何 USN 调用返回错误 → 回退，并把 `next_usn` 清零（下次必然全巡）。

## 5. 为什么不在索引条目里存 FRN

存 FRN 可以让增量刷新直接定位到条目，但会让缓存格式升版、且 FRN 在文件移动后会变，
维护成本与假阴性风险都高。第一版只把 USN 用作**「哪些目录脏了」的来源**，
后续的目录重枚举沿用现有 `refresh_in_place(hint)` 路径 —— 语义与监听推来的脏目录完全一致。

## 6. 接入点

`refresh_with(index)` 现在是：

```
if !NEED_FULL_SCAN { 应用 DIRTY_DIRS → refresh_in_place(Some(dirty)) }
else { refresh_in_place(None) /* 全盘巡 */ ; save_cache }
```

改为：

```
if !NEED_FULL_SCAN { ... 不变 ... }
else if let Some(dirty) = usn::incremental_since(&drives) {
    refresh_in_place(Some(dirty));            // 复用现有增量路径
    let saved = save_cache(index);
    usn::commit_snapshot(built_unix, &drives, &checkpoints, saved);
} else {
    refresh_in_place(None);                   // 原全盘巡
    let saved = save_cache(index);
    usn::commit_full_sweep(&drives, built_unix, saved);
}
```

### 6.1 落地时的两处修正（2026-11-06）

蓝图原写法有个没考虑到的问题：**GUI 与每个 MCP 进程各持一份内存索引**。
如果游标只存在文件里、每次刷新都跟文件比，两个进程会互相把对方的状态判成
「对不上」，陷入「你全巡一次、我全巡一次」的乒乓，等于没优化。

实际实现改成：

1. **游标以进程内存为准**（`CURSORS`）。USN 是卷级全局日志，每个进程从自己
   上次确认的位置往后读，就能拿到这段时间的全部变更（包括别的进程造成的）。
   进程内游标与进程内索引严格同步推进，天然没有乒乓。
2. **状态文件只是暖启动提示**。`usn-state.json` 里记 `cache_unix`，只有它等于
   刚载入的那份缓存的 `built_unix` 时才采纳游标（`adopt_state`）。对不上就
   `None` → 全盘巡，不会拿旧游标去配另一份索引。
3. **只有缓存真的落盘了才写状态文件**（`save_cache` 改成返回 bool）。写失败时
   内存游标照常推进，下次进程启动对不上 → 全盘巡，安全。
4. `usn.rs` 的接口按图定名：`incremental_since` / `commit_snapshot` /
   `commit_full_sweep` / `adopt_state`。

监听快路径（`NEED_FULL_SCAN=false`）**不推进** USN 游标：那条路径没有查日志，
游标落后只会导致下次多读一段已处理过的记录（幂等），不会漏。

## 7. 验证门槛（不过就不合并）

- 单测：日志 ID 变化 / USN 过旧 / FRN 解析失败 三种情况都必须返回「回退」。
- 实机：故意新建/改名/删除文件后重启进程，首查必须命中最新的那批文件。
- 对照：同一组查询在「USN 路径」与「全盘巡路径」下结果集逐字节一致。
- 性能：首查从 17–22s 降到 < 2s。

## 9. 实机调试记录（2026-11-06）

**已打通**：开卷、`FSCTL_QUERY_USN_JOURNAL`、`FSCTL_READ_USN_JOURNAL`（`READ_USN_JOURNAL_DATA_V1`，Min/MaxMajorVersion=2），
在 C 盘与 D 盘都成功读出过记录（D 盘一次读出 51,815 条）。

**踩到的坑（务必写进实现）**：

1. `ERROR_INVALID_PARAMETER(87)` 不一定是参数写错，**多半是 `StartUsn` 落在该盘当前有效区间之外**。
   - 用 `next_usn - 2000` 当起点：C 盘成功，D 盘 87。
   - 用 `lowest_valid`(0) 当起点：D 盘成功，C 盘 87。
   - 结论：**起点必须用日志自己报告的 `FirstUsn` 校验**（`start >= FirstUsn`），
     `LowestValidUsn` 不足以判断——D 盘 32MB 日志写入量大，回卷很快。
2. 因此 `JournalInfo` 必须带上 `FirstUsn`，`can_resume()` 的三条判定改成：
   `journal_id 一致 && start_usn >= FirstUsn && start_usn <= NextUsn`。
3. 读失败一律返回 None → 回退全盘巡，这条设计已在代码里生效。

## 10. 提权（最后一步，单独做）

读卷需要管理员。改法：`HKCU\...\Run` → 计划任务（登录时触发、`RunLevel Highest`）。
代价：托盘程序以管理员常驻；资源管理器拖拽到提权窗口会被 UIPI 拦住。
**在 §7 全部通过之前不切**，否则只是徒增风险。

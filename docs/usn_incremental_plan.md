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
else if let Some(dirty) = usn::dirty_dirs_since_snapshot() {
    refresh_in_place(Some(dirty));            // 复用现有增量路径
    usn::commit_snapshot();                   // 记下 next_usn
    save_cache(index);
} else {
    refresh_in_place(None);                   // 原全盘巡
    usn::commit_full_sweep();                 // 记下 journal_id + next_usn
    save_cache(index);
}
```

## 7. 验证门槛（不过就不合并）

- 单测：日志 ID 变化 / USN 过旧 / FRN 解析失败 三种情况都必须返回「回退」。
- 实机：故意新建/改名/删除文件后重启进程，首查必须命中最新的那批文件。
- 对照：同一组查询在「USN 路径」与「全盘巡路径」下结果集逐字节一致。
- 性能：首查从 17–22s 降到 < 2s。

## 8. 提权（最后一步，单独做）

读卷需要管理员。改法：`HKCU\...\Run` → 计划任务（登录时触发、`RunLevel Highest`）。
代价：托盘程序以管理员常驻；资源管理器拖拽到提权窗口会被 UIPI 拦住。
**在 §7 全部通过之前不切**，否则只是徒增风险。

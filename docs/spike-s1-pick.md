# Spike S1：全屏 overlay 下如何拾取"下层"窗口 / UI 元素

> 对应开发计划 1030 的 P0-1。本轮实现已完成，本文记录方案选择与实测结论。

## 1. 结论

采用 **S1-a（临时穿透）为主 + S1-b（自身类名排除）兜底**：

1. `PickEngine::probe` 在探测前对 overlay 设置 `WS_EX_TRANSPARENT`，调用
   `WindowFromPoint` / UIA `ElementFromPoint`，随后立即恢复原样式（幂等）。
2. 对返回的窗口再按 `PixShot*` 类名排除自身（工具条 / 贴图 / 菜单 / 颜色面板）。
3. UIA 不可用、或 `ElementFromPoint` 返回 overlay 自身时，自动退化为纯窗口级
   （`TargetKind::Fallback`），不阻塞框选流程。

## 2. 主要实现

| 位置 | 内容 |
|------|------|
| `src/pick.rs` | `PickLevel` / `PickTarget` / `PickEngine`、`WindowFromPoint`、DWM 扩展边框、UIA 元素与父子导航、`normalize`/`contains`/`truncate_label` |
| `src/overlay.rs` | `Selecting` 阶段悬停探测（40ms 节流）、`R/W/E` 级别切换、`Tab` 父/子、`Enter` 提交、单击自动扩充、`last_pick` 脏区擦除 |
| `src/overlay.rs` | `begin_annotating()` 抽取：`finish_selection` 与候选框提交共用同一条裁图/建会话路径 |
| `src/config.rs` | `[pick]` 段（level / padding / auto_expand / interval / label / max_depth） |

## 3. 实测

- 单元测试：`normalize` 越界裁剪/零尺寸、`contains` 左闭右开、标签头尾保留、配置回退，
  全部通过。
- GUI E2E（`scripts/gui_e2e_features_test.ps1`）：默认 `pick_level=region` 时
  **不拖动、单击**即可自动扩充为光标下窗口并进入标注，通过。
- 未覆盖：100%/150%/200% DPI 命中率统计、双屏负坐标、管理员窗口 UWP、Win7 精简版；
  这些需要目标机手工矩阵（见 `docs/qa-1030.md` 的待办项）。

## 4. 风险与缓解（落地后）

- overlay 临时改样式可能带来 1 帧闪烁：`Region` 级别**不在悬停时探测**（仅单击时探测），
  避免默认配置下每帧改样式；`Window/Element` 才按 40ms 节流探测。
- UIA 深树超时：`deepest_child` 限制深度 12、每层最多 256 个兄弟，超限返回已得层级。
- UIPI：UIA `E_ACCESSDENIED` 时 `element_at` 返回 `None` → 窗口级兜底，不崩溃。

## 5. 后续（未在本轮完成）

- 真实命中率/P95 耗时采集表（需要目标机自动化）。
- UIA `GetRuntimeId` 缓存（当前 `runtime_id` 预留为空）。
- 元素级标签的控件类型本地化（当前显示"元素 · 名称"）。

//! 标注引擎：文档与命令栈（纯逻辑，不依赖 Win32）。
//!
//! `AnnotDoc` 持有全部标注（下标即 z 序）+ 撤销/重做命令栈 + 只增不减的步骤计数器
//! （见设计文档 3.2 / 3.3 / 5.4）。移动/缩放统一为几何级 `Transform`：拖拽期间只在
//! mouseup 时生成一条命令，天然合并整段拖拽。撤销栈深度上限 [`UNDO_LIMIT`]。

use std::collections::VecDeque;

use super::geometry::{rect_union, Pt, Rect};
use super::shape::{Annotation, Shape};

/// 撤销栈深度上限（超出丢弃最旧记录）：长会话内存有界。
const UNDO_LIMIT: usize = 200;

/// 单个形状的几何变更（一次变换可能含多个形状——挂线文字随线段一起动）。
#[derive(Debug, Clone)]
pub struct ShapeChange {
    pub id: u64,
    pub old: Shape,
    pub new: Shape,
}

/// 可撤销的一步操作。
#[derive(Debug, Clone)]
pub enum Command {
    Add(Annotation),
    /// 整组删除（挂线：线段/箭头连同挂线文字一次删除、一步撤销）。
    /// `items` 按删除前的原始下标**升序**记录；撤销时按序插回即恢复原 z 序。
    DeleteGroup { items: Vec<(usize, Annotation)> },
    /// 几何变换：记旧/新形状（不做增量 delta）。可含多个形状以合并成一步。
    Transform { changes: Vec<ShapeChange> },
    /// 只替换形状本身，**不触发** attach 重排/解绑逻辑（颜色面板用）。
    Restyle { id: u64, old: Shape, new: Shape },
}

/// 标注文档。
pub struct AnnotDoc {
    /// 下标即 z 序；绘制、命中均按此序。
    shapes: Vec<Annotation>,
    next_id: u64,
    /// 步骤序号计数器：只增不减（见 5.4）。
    step_counter: u32,
    undo: VecDeque<Command>,
    redo: Vec<Command>,
}

impl Default for AnnotDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl AnnotDoc {
    pub fn new() -> Self {
        Self {
            shapes: Vec::new(),
            next_id: 1,
            step_counter: 1,
            undo: VecDeque::new(),
            redo: Vec::new(),
        }
    }

    pub fn shapes(&self) -> &[Annotation] {
        &self.shapes
    }

    #[allow(dead_code)] // 供测试与后续功能区使用
    pub fn len(&self) -> usize {
        self.shapes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// 按 id 查找标注在 vec 中的下标（即 z 序）。
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.shapes.iter().position(|a| a.id == id)
    }

    /// 按 id 取形状。
    pub fn find(&self, id: u64) -> Option<&Shape> {
        self.shapes.iter().find(|a| a.id == id).map(|a| &a.shape)
    }

    #[allow(dead_code)] // 供测试与后续功能区使用
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    #[allow(dead_code)] // 供测试与后续功能区使用
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 下一个步骤序号（不消费）——供绘制预览时显示将要使用的序号。
    pub fn peek_step_number(&self) -> u32 {
        self.step_counter
    }

    /// 取用下一个步骤序号（消费）；计数器只增不减。
    pub fn take_step_number(&mut self) -> u32 {
        let n = self.step_counter;
        self.step_counter += 1;
        n
    }

    /// 入栈一步操作（清空 redo；超上限丢弃最旧记录）。
    fn push_undo(&mut self, cmd: Command) {
        if self.undo.len() >= UNDO_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(cmd);
        self.redo.clear();
    }

    /// 新增标注：分配 id、入 `Add` 命令、清空 redo。返回新 id。
    pub fn add(&mut self, shape: Shape) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let ann = Annotation { id, shape };
        self.shapes.push(ann.clone());
        self.push_undo(Command::Add(ann));
        id
    }

    /// 删除标注；返回被删形状的包围盒（脏区）。
    ///
    /// **整组删除**（挂线）：删除线段/箭头时，挂在其上的文字一并删除——
    /// 整组记为**一条** `DeleteGroup` 命令，一次撤销全部恢复（见 [`Shape::Text`]
    /// 的 `attach`）。
    pub fn delete(&mut self, id: u64) -> Option<Rect> {
        // 目标本身必须存在（防御）
        self.index_of(id)?;
        // 目标 + 挂在目标上的文字；记录**移除前**的原始下标（升序）
        let mut targets = vec![id];
        targets.extend(
            self.shapes
                .iter()
                .filter(|a| matches!(&a.shape, Shape::Text { attach: Some(x), .. } if *x == id))
                .map(|a| a.id),
        );
        let mut items: Vec<(usize, Annotation)> = targets
            .iter()
            .filter_map(|t| {
                let i = self.index_of(*t)?;
                Some((i, self.shapes[i].clone()))
            })
            .collect();
        items.sort_by_key(|(i, _)| *i);
        // 按下标倒序移除，避免下标失效
        let mut dirty: Option<Rect> = None;
        for (_, ann) in items.iter().rev() {
            if let Some(i) = self.index_of(ann.id) {
                let ann = self.shapes.remove(i);
                let d = ann.shape.bbox();
                dirty = Some(match dirty {
                    Some(prev) => rect_union(prev, d),
                    None => d,
                });
            }
        }
        self.push_undo(Command::DeleteGroup { items });
        dirty
    }

    /// 修改样式：保留 Text/Watermark 的 `attach` 与 `origin`，仅替换样式相关字段
    ///（由调用方保证"克隆原形状后只改样式"）。返回新旧包围盒的并集（脏区）。
    ///
    /// **禁止**用 [`transform`](Self::transform) 改样式：它会解除文字挂线。
    pub fn restyle(&mut self, id: u64, new: Shape) -> Option<Rect> {
        let i = self.index_of(id)?;
        let old = self.shapes[i].shape.clone();
        debug_assert_eq!(
            std::mem::discriminant(&old),
            std::mem::discriminant(&new),
            "restyle 只允许同类型替换（调用方 bug）"
        );
        let dirty = rect_union(old.bbox(), new.bbox());
        self.shapes[i].shape = new.clone();
        self.push_undo(Command::Restyle { id, old, new });
        Some(dirty)
    }

    /// 批量删除：自动去重、自动连带删除挂在被删形状上的文字。
    /// 一次调用 = 一条撤销记录。
    pub fn delete_many(&mut self, ids: &[u64]) -> Option<Rect> {
        // 去重 + 限定存在的 id
        let mut uniq: Vec<u64> = Vec::with_capacity(ids.len());
        for id in ids {
            if self.index_of(*id).is_some() && !uniq.contains(id) {
                uniq.push(*id);
            }
        }
        if uniq.is_empty() {
            return None;
        }
        // 连带挂线文字
        let mut targets = uniq.clone();
        for a in &self.shapes {
            if let Shape::Text { attach: Some(x), .. } = &a.shape {
                if uniq.contains(x) && !targets.contains(&a.id) {
                    targets.push(a.id);
                }
            }
        }
        let mut items: Vec<(usize, Annotation)> = targets
            .iter()
            .filter_map(|t| self.index_of(*t).map(|i| (i, self.shapes[i].clone())))
            .collect();
        items.sort_by_key(|(i, _)| *i);
        if items.is_empty() {
            return None;
        }
        let mut dirty: Option<Rect> = None;
        for (_, ann) in &items {
            dirty = Some(match dirty {
                Some(prev) => rect_union(prev, ann.shape.bbox()),
                None => ann.shape.bbox(),
            });
        }
        for (index, _) in items.iter().rev() {
            if *index < self.shapes.len() {
                self.shapes.remove(*index);
            }
        }
        self.push_undo(Command::DeleteGroup { items });
        dirty
    }

    /// 清空全部标注（一条撤销记录）；空文档返回 `None`，不产生撤销记录。
    pub fn clear_all(&mut self) -> Option<Rect> {
        if self.shapes.is_empty() {
            return None;
        }
        let items: Vec<(usize, Annotation)> = self.shapes.iter().cloned().enumerate().collect();
        let mut dirty: Option<Rect> = None;
        for (_, ann) in &items {
            dirty = Some(match dirty {
                Some(prev) => rect_union(prev, ann.shape.bbox()),
                None => ann.shape.bbox(),
            });
        }
        self.shapes.clear();
        self.push_undo(Command::DeleteGroup { items });
        dirty
    }

    /// 几何变换（移动/缩放）；`old` 为变换前的形状。返回新旧包围盒的并集（脏区）。
    ///
    /// 挂线联动：变换线段/箭头时，挂在其上的文字重新居中到新中点（合并为同一步）；
    /// 变换文字本身（= 移动）则解除挂线，避免"拖走后仍被线拽回"。
    pub fn transform(&mut self, id: u64, old: Shape, new: Shape) -> Option<Rect> {
        if self.index_of(id).is_none() {
            return None;
        }
        debug_assert_eq!(
            self.find(id),
            Some(&old),
            "transform 的 old 必须与文档当前值一致（否则撤销栈被污染）"
        );
        let mut changes = vec![ShapeChange {
            id,
            old: old.clone(),
            new: new.clone(),
        }];
        // 挂线：文字被移动 → 脱离
        if let Shape::Text { attach: Some(_), .. } = &new {
            if let Shape::Text { attach, .. } = &mut changes[0].new {
                *attach = None;
            }
        }
        // 挂线：线段/箭头被移动或缩放 → 依附其上的文字重新居中
        if let Some(mid) = seg_mid(&new) {
            let attached: Vec<(u64, Shape)> = self
                .shapes
                .iter()
                .filter(|a| a.id != id)
                .filter(|a| matches!(&a.shape, Shape::Text { attach: Some(x), .. } if *x == id))
                .map(|a| (a.id, a.shape.clone()))
                .collect();
            for (tid, cur) in attached {
                let moved = center_text_on(cur.clone(), mid);
                if moved != cur {
                    changes.push(ShapeChange {
                        id: tid,
                        old: cur,
                        new: moved,
                    });
                }
            }
        }
        let mut dirty: Option<Rect> = None;
        for c in &changes {
            let d = rect_union(c.old.bbox(), c.new.bbox());
            dirty = Some(match dirty {
                Some(prev) => rect_union(prev, d),
                None => d,
            });
            if let Some(i) = self.index_of(c.id) {
                self.shapes[i].shape = c.new.clone();
            }
        }
        self.push_undo(Command::Transform { changes });
        dirty
    }

    /// 撤销一步；返回受影响区域的包围盒。
    pub fn undo(&mut self) -> Option<Rect> {
        let cmd = self.undo.pop_back()?;
        let dirty = self.apply_undo(&cmd);
        self.redo.push(cmd);
        Some(dirty)
    }

    /// 重做一步；返回受影响区域的包围盒。
    pub fn redo(&mut self) -> Option<Rect> {
        let cmd = self.redo.pop()?;
        let dirty = self.apply_redo(&cmd);
        self.undo.push_back(cmd);
        Some(dirty)
    }

    fn apply_undo(&mut self, cmd: &Command) -> Rect {
        match cmd {
            Command::Add(ann) => {
                if let Some(i) = self.index_of(ann.id) {
                    self.shapes.remove(i);
                }
                ann.shape.bbox()
            }
            Command::DeleteGroup { items } => {
                // 按原始下标升序插回，恢复原 z 序
                let mut dirty: Option<Rect> = None;
                for (index, ann) in items {
                    let i = (*index).min(self.shapes.len());
                    self.shapes.insert(i, ann.clone());
                    let d = ann.shape.bbox();
                    dirty = Some(match dirty {
                        Some(prev) => rect_union(prev, d),
                        None => d,
                    });
                }
                dirty.unwrap_or(Rect::new(0, 0, 0, 0))
            }
            Command::Transform { changes } => {
                let mut dirty: Option<Rect> = None;
                for c in changes {
                    if let Some(i) = self.index_of(c.id) {
                        self.shapes[i].shape = c.old.clone();
                    }
                    let d = rect_union(c.old.bbox(), c.new.bbox());
                    dirty = Some(match dirty {
                        Some(prev) => rect_union(prev, d),
                        None => d,
                    });
                }
                dirty.unwrap_or(Rect::new(0, 0, 0, 0))
            }
            Command::Restyle { id, old, new } => {
                if let Some(i) = self.index_of(*id) {
                    self.shapes[i].shape = old.clone();
                }
                rect_union(old.bbox(), new.bbox())
            }
        }
    }

    fn apply_redo(&mut self, cmd: &Command) -> Rect {
        match cmd {
            Command::Add(ann) => {
                self.shapes.push(ann.clone());
                ann.shape.bbox()
            }
            Command::DeleteGroup { items } => {
                // 按 id 移除（不依赖下标与历史严格吻合，更稳健）
                let mut dirty: Option<Rect> = None;
                for (_, ann) in items {
                    if let Some(i) = self.index_of(ann.id) {
                        self.shapes.remove(i);
                    }
                    let d = ann.shape.bbox();
                    dirty = Some(match dirty {
                        Some(prev) => rect_union(prev, d),
                        None => d,
                    });
                }
                dirty.unwrap_or(Rect::new(0, 0, 0, 0))
            }
            Command::Transform { changes } => {
                let mut dirty: Option<Rect> = None;
                for c in changes {
                    if let Some(i) = self.index_of(c.id) {
                        self.shapes[i].shape = c.new.clone();
                    }
                    let d = rect_union(c.old.bbox(), c.new.bbox());
                    dirty = Some(match dirty {
                        Some(prev) => rect_union(prev, d),
                        None => d,
                    });
                }
                dirty.unwrap_or(Rect::new(0, 0, 0, 0))
            }
            Command::Restyle { id, old, new } => {
                if let Some(i) = self.index_of(*id) {
                    self.shapes[i].shape = new.clone();
                }
                rect_union(old.bbox(), new.bbox())
            }
        }
    }
}

/// 线段/箭头的中点（挂线文字的落点）；非线段类返回 `None`。
/// 中点按 `from + (to-from)/2` 计算，避免 `from+to` 在极端坐标下溢出。
pub(crate) fn seg_mid(shape: &Shape) -> Option<Pt> {
    match shape {
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => Some(Pt::new(
            from.x + (to.x - from.x) / 2,
            from.y + (to.y - from.y) / 2,
        )),
        _ => None,
    }
}

/// 把文字（`bbox` 中心）移到 `c` 处，返回新形状；非文字原样返回。
fn center_text_on(shape: Shape, c: Pt) -> Shape {
    match shape {
        Shape::Text {
            text,
            bbox,
            style,
            frame,
            attach,
            ..
        } => Shape::Text {
            origin: Pt::new(c.x - bbox.w / 2, c.y - bbox.h / 2),
            text,
            bbox: Rect::new(c.x - bbox.w / 2, c.y - bbox.h / 2, bbox.w, bbox.h),
            style,
            frame,
            attach,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::style::{Rgba, StrokeStyle, DEF_BADGE, DEF_STROKE};

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Shape {
        Shape::Rect {
            bbox: Rect::new(x, y, w, h),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 3,
            },
            fill: None,
        }
    }

    fn step(n: u32) -> Shape {
        Shape::StepNo {
            center: Pt::new(0, 0),
            radius: 14,
            number: n,
            style: DEF_BADGE.clone(),
        }
    }

    fn line(from: Pt, to: Pt) -> Shape {
        Shape::Line {
            from,
            to,
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 3,
            },
        }
    }

    fn text(cx: i32, cy: i32, attach: Option<u64>) -> Shape {
        Shape::Text {
            origin: Pt::new(cx - 10, cy - 8),
            text: "hi".into(),
            bbox: Rect::new(cx - 10, cy - 8, 20, 16),
            style: super::super::style::TextStyle {
                color: DEF_STROKE,
                font_px: 16,
                bold: false,
                italic: false,
                outline: false,
            },
            frame: super::super::style::TextFrame::None,
            attach,
        }
    }

    #[test]
    fn add_assigns_increasing_ids_and_undo_redo() {
        let mut d = AnnotDoc::new();
        let a = d.add(rect(0, 0, 10, 10));
        let b = d.add(rect(20, 20, 10, 10));
        assert_eq!(d.len(), 2);
        assert_eq!((a, b), (1, 2));

        d.undo();
        assert_eq!(d.len(), 1);
        d.undo();
        assert_eq!(d.len(), 0);
        assert!(d.is_empty());
        d.redo();
        assert_eq!(d.len(), 1);
        d.redo();
        assert_eq!(d.len(), 2);
        // 重做后仍在末尾（z 序恢复）
        assert_eq!(d.shapes()[1].id, b);
    }

    #[test]
    fn add_clears_redo() {
        let mut d = AnnotDoc::new();
        d.add(rect(0, 0, 4, 4));
        d.undo();
        assert!(d.can_redo());
        d.add(rect(1, 1, 4, 4));
        assert!(!d.can_redo(), "新增后必须清空 redo");
    }

    #[test]
    fn delete_restores_index_on_undo() {
        let mut d = AnnotDoc::new();
        let a = d.add(rect(0, 0, 4, 4));
        let b = d.add(rect(5, 5, 4, 4));
        let c = d.add(rect(9, 9, 4, 4));
        assert!(d.delete(b).is_some());
        assert_eq!(d.shapes().iter().map(|x| x.id).collect::<Vec<_>>(), vec![a, c]);
        d.undo();
        // b 应按原 index（1）插回，z 序恢复 a,b,c
        assert_eq!(d.shapes().iter().map(|x| x.id).collect::<Vec<_>>(), vec![a, b, c]);
    }

    #[test]
    fn transform_writes_back_old_on_undo() {
        let mut d = AnnotDoc::new();
        let id = d.add(rect(0, 0, 10, 10));
        let old = d.find(id).unwrap().clone();
        let new = old.translate(Pt::new(5, 5));
        d.transform(id, old.clone(), new.clone());
        assert_eq!(d.find(id), Some(&new));
        d.undo();
        assert_eq!(d.find(id), Some(&old));
        d.redo();
        assert_eq!(d.find(id), Some(&new));
    }

    #[test]
    fn step_counter_only_increases() {
        let mut d = AnnotDoc::new();
        assert_eq!(d.take_step_number(), 1);
        assert_eq!(d.take_step_number(), 2);
        let two = d.add(step(2));
        // 删除「序号 2」后，下一个新序号仍是 3（不回收）
        d.delete(two);
        assert_eq!(d.peek_step_number(), 3);
        // 撤销删除后计数器仍是 3
        d.undo();
        assert_eq!(d.peek_step_number(), 3);
        assert_eq!(d.find(two).is_some(), true);
    }

    #[test]
    fn empty_undo_redo_is_none() {
        let mut d = AnnotDoc::new();
        assert!(d.undo().is_none());
        assert!(d.redo().is_none());
    }

    #[test]
    fn moving_line_recenters_attached_text_in_one_step() {
        let mut d = AnnotDoc::new();
        let lid = d.add(line(Pt::new(0, 0), Pt::new(100, 0))); // 中点 (50,0)
        let tid = d.add(text(50, 0, Some(lid)));
        // 移动线段 +40 x：文字应随中点移到 (90,0)
        let old = d.find(lid).unwrap().clone();
        let new = old.translate(Pt::new(40, 0));
        d.transform(lid, old, new);
        match d.find(tid).unwrap() {
            Shape::Text { bbox, .. } => assert_eq!(bbox.center(), Pt::new(90, 0)),
            other => panic!("应为文字，得到 {other:?}"),
        }
        // 一步撤销：文字与线段一起回退
        d.undo();
        match d.find(tid).unwrap() {
            Shape::Text { bbox, .. } => assert_eq!(bbox.center(), Pt::new(50, 0)),
            other => panic!("撤销后应为原位置，得到 {other:?}"),
        }
        match d.find(lid).unwrap() {
            Shape::Line { from, .. } => assert_eq!(*from, Pt::new(0, 0)),
            other => panic!("线段应回退，得到 {other:?}"),
        }
    }

    #[test]
    fn moving_text_detaches_it() {
        let mut d = AnnotDoc::new();
        let lid = d.add(line(Pt::new(0, 0), Pt::new(100, 0)));
        let tid = d.add(text(50, 0, Some(lid)));
        let old = d.find(tid).unwrap().clone();
        let new = old.translate(Pt::new(0, 30));
        d.transform(tid, old, new);
        match d.find(tid).unwrap() {
            Shape::Text { attach, .. } => assert_eq!(*attach, None, "拖动文字应解除挂线"),
            other => panic!("应为文字，得到 {other:?}"),
        }
    }

    #[test]
    fn deleting_line_deletes_attached_text_group() {
        let mut d = AnnotDoc::new();
        let lid = d.add(line(Pt::new(0, 0), Pt::new(100, 0)));
        let tid = d.add(text(50, 0, Some(lid)));
        let other = d.add(rect(200, 200, 10, 10));
        d.delete(lid);
        // 线段与挂线文字都删掉，无关的矩形保留
        assert!(d.find(lid).is_none());
        assert!(d.find(tid).is_none());
        assert!(d.find(other).is_some());
        // 整组删除 = 一条命令：一次撤销全部恢复（且恢复原 z 序：线在文字之下）
        d.undo();
        assert!(d.find(lid).is_some());
        assert!(d.find(tid).is_some());
        assert_eq!(
            d.shapes().iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![lid, tid, other],
            "撤销后 z 序必须恢复"
        );
        // 一次重做整组再删
        d.redo();
        assert!(d.find(lid).is_none());
        assert!(d.find(tid).is_none());
        assert!(d.find(other).is_some());
    }

    #[test]
    fn undo_stack_is_bounded() {
        let mut d = AnnotDoc::new();
        for i in 0..(UNDO_LIMIT + 50) {
            d.add(rect(i as i32, 0, 2, 2));
        }
        let mut n = 0;
        while d.undo().is_some() {
            n += 1;
        }
        assert_eq!(n, UNDO_LIMIT, "撤销步数不超过上限（最旧记录被丢弃）");
        // 最早的 50 条已被挤出：文档还剩 50 个形状
        assert_eq!(d.len(), 50);
    }

    #[test]
    fn restyle_rect_color_and_undo() {
        let mut d = AnnotDoc::new();
        let id = d.add(rect(0, 0, 10, 10));
        let old = d.find(id).unwrap().clone();
        let Shape::Rect { bbox, fill, .. } = old else { panic!() };
        let new = Shape::Rect {
            bbox,
            style: StrokeStyle {
                color: Rgba::new(0x00, 0x80, 0xFF, 0xFF),
                width: 3,
            },
            fill,
        };
        let dirty = d.restyle(id, new.clone()).unwrap();
        assert_eq!(d.find(id), Some(&new));
        assert_eq!(dirty, rect(0, 0, 10, 10).bbox(), "脏区并集");
        d.undo();
        assert_eq!(d.find(id), Some(&Shape::Rect {
            bbox: old.bbox(),
            style: StrokeStyle {
                color: DEF_STROKE,
                width: 3,
            },
            fill: None,
        }));
    }

    #[test]
    fn restyle_text_preserves_attach() {
        let mut d = AnnotDoc::new();
        let lid = d.add(line(Pt::new(0, 0), Pt::new(100, 0)));
        let tid = d.add(text(50, 0, Some(lid)));
        let old = d.find(tid).unwrap().clone();
        let Shape::Text { origin, text, .. } = old else { panic!() };
        let new = Shape::Text {
            origin,
            text,
            bbox: Rect::new(40, -8, 20, 16),
            style: super::super::style::TextStyle {
                color: super::super::style::DEF_STROKE,
                font_px: 32,
                bold: true,
                italic: false,
                outline: false,
            },
            frame: super::super::style::TextFrame::None,
            attach: Some(lid),
        };
        d.restyle(tid, new.clone()).unwrap();
        match d.find(tid).unwrap() {
            Shape::Text { attach, .. } => assert_eq!(*attach, Some(lid), "改样式不得解除挂线"),
            _ => panic!(),
        }
        d.undo();
        assert!(matches!(
            d.find(tid).unwrap(),
            Shape::Text { attach: Some(x), .. } if *x == lid
        ));
    }

    #[test]
    fn delete_many_dedups_and_deletes_attached_text() {
        let mut d = AnnotDoc::new();
        let a = d.add(rect(0, 0, 10, 10));
        let b = d.add(rect(20, 20, 10, 10));
        let lid = d.add(line(Pt::new(0, 50), Pt::new(100, 50)));
        let tid = d.add(text(50, 50, Some(lid)));
        let dirty = d.delete_many(&[a, a, b, lid]).unwrap();
        assert!(d.find(a).is_none() && d.find(b).is_none());
        assert!(d.find(lid).is_none() && d.find(tid).is_none());
        assert!(dirty.intersects(rect(0, 0, 10, 10).bbox()));
        d.undo();
        assert_eq!(d.len(), 4, "一次撤销全部恢复");
    }

    #[test]
    fn clear_all_is_one_undo_and_empty_is_none() {
        let mut d = AnnotDoc::new();
        assert_eq!(d.clear_all(), None);
        d.add(rect(0, 0, 10, 10));
        d.add(rect(20, 20, 10, 10));
        d.add(rect(40, 40, 10, 10));
        let dirty = d.clear_all().unwrap();
        assert!(d.is_empty());
        assert!(dirty.w > 0);
        d.undo();
        assert_eq!(d.len(), 3, "一次撤销恢复全部");
        d.redo();
        assert!(d.is_empty(), "一次重做再次清空（clear_all 只占 1 格）");
    }
}

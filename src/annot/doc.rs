//! 标注引擎：文档与命令栈（纯逻辑，不依赖 Win32）。
//!
//! `AnnotDoc` 持有全部标注（下标即 z 序）+ 撤销/重做命令栈 + 只增不减的步骤计数器
//! （见设计文档 3.2 / 3.3 / 5.4）。移动/缩放统一为几何级 `Transform`：拖拽期间只在
//! mouseup 时生成一条命令，天然合并整段拖拽。

use super::geometry::{rect_union, Pt, Rect};
use super::shape::{Annotation, Shape};

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
    /// `index` = 删除时的 z 序，撤销时按此插回。
    Delete { index: usize, ann: Annotation },
    /// 几何变换：记旧/新形状（不做增量 delta）。可含多个形状以合并成一步。
    Transform { changes: Vec<ShapeChange> },
}

/// 标注文档。
pub struct AnnotDoc {
    /// 下标即 z 序；绘制、命中均按此序。
    shapes: Vec<Annotation>,
    next_id: u64,
    /// 步骤序号计数器：只增不减（见 5.4）。
    step_counter: u32,
    undo: Vec<Command>,
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
            undo: Vec::new(),
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

    /// 新增标注：分配 id、入 `Add` 命令、清空 redo。返回新 id。
    pub fn add(&mut self, shape: Shape) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let ann = Annotation { id, shape };
        self.shapes.push(ann.clone());
        self.undo.push(Command::Add(ann));
        self.redo.clear();
        id
    }

    /// 删除标注；返回被删形状的包围盒（脏区）。
    ///
    /// **整组删除**（挂线）：删除线段/箭头时，挂在其上的文字一并删除——它们的 Delete
    /// 命令各自入栈，撤销时逐个恢复（见 [`Shape::Text`] 的 `attach`）。
    pub fn delete(&mut self, id: u64) -> Option<Rect> {
        let mut dirty: Option<Rect> = None;
        // 先删挂在目标上的文字，再删目标（多次删除按 id 收集，倒序移除避免下标失效）
        let mut targets = vec![id];
        targets.extend(
            self.shapes
                .iter()
                .filter(|a| matches!(&a.shape, Shape::Text { attach: Some(x), .. } if *x == id))
                .map(|a| a.id),
        );
        // 目标本身可能不存在（防御）
        if self.index_of(id).is_none() {
            return None;
        }
        for t in targets {
            let Some(index) = self.index_of(t) else {
                continue;
            };
            let ann = self.shapes.remove(index);
            let d = ann.shape.bbox();
            dirty = Some(match dirty {
                Some(prev) => rect_union(prev, d),
                None => d,
            });
            self.undo.push(Command::Delete { index, ann });
            self.redo.clear();
        }
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
        self.undo.push(Command::Transform { changes });
        self.redo.clear();
        dirty
    }

    /// 撤销一步；返回受影响区域的包围盒。
    pub fn undo(&mut self) -> Option<Rect> {
        let cmd = self.undo.pop()?;
        let dirty = self.apply_undo(&cmd);
        self.redo.push(cmd);
        Some(dirty)
    }

    /// 重做一步；返回受影响区域的包围盒。
    pub fn redo(&mut self) -> Option<Rect> {
        let cmd = self.redo.pop()?;
        let dirty = self.apply_redo(&cmd);
        self.undo.push(cmd);
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
            Command::Delete { index, ann } => {
                let i = (*index).min(self.shapes.len());
                self.shapes.insert(i, ann.clone());
                ann.shape.bbox()
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
        }
    }

    fn apply_redo(&mut self, cmd: &Command) -> Rect {
        match cmd {
            Command::Add(ann) => {
                self.shapes.push(ann.clone());
                ann.shape.bbox()
            }
            Command::Delete { index, ann } => {
                let i = (*index).min(self.shapes.len().saturating_sub(1));
                if !self.shapes.is_empty() {
                    self.shapes.remove(i);
                }
                ann.shape.bbox()
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
        }
    }
}

/// 线段/箭头的中点（挂线文字的落点）；非线段类返回 `None`。
fn seg_mid(shape: &Shape) -> Option<Pt> {
    match shape {
        Shape::Line { from, to, .. } | Shape::Arrow { from, to, .. } => {
            Some(Pt::new((from.x + to.x) / 2, (from.y + to.y) / 2))
        }
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
    use super::super::style::{DEF_BADGE, DEF_STROKE};
    use super::super::style::StrokeStyle;

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
        // 撤销逐个恢复
        d.undo();
        d.undo();
        assert!(d.find(lid).is_some());
        assert!(d.find(tid).is_some());
    }
}

//! 标注引擎：文档与命令栈（纯逻辑，不依赖 Win32）。
//!
//! `AnnotDoc` 持有全部标注（下标即 z 序）+ 撤销/重做命令栈 + 只增不减的步骤计数器
//! （见设计文档 3.2 / 3.3 / 5.4）。移动/缩放统一为几何级 `Transform`：拖拽期间只在
//! mouseup 时生成一条命令，天然合并整段拖拽。

use super::geometry::{rect_union, Rect};
use super::shape::{Annotation, Shape};

/// 可撤销的一步操作。
#[derive(Debug, Clone)]
pub enum Command {
    Add(Annotation),
    /// `index` = 删除时的 z 序，撤销时按此插回。
    Delete { index: usize, ann: Annotation },
    /// 几何变换：记旧/新形状（不做增量 delta）。
    Transform { id: u64, old: Shape, new: Shape },
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
    pub fn delete(&mut self, id: u64) -> Option<Rect> {
        let index = self.index_of(id)?;
        let ann = self.shapes.remove(index);
        let dirty = ann.shape.bbox();
        self.undo.push(Command::Delete { index, ann });
        self.redo.clear();
        Some(dirty)
    }

    /// 几何变换（移动/缩放）；`old` 为变换前的形状。返回新旧包围盒的并集（脏区）。
    pub fn transform(&mut self, id: u64, old: Shape, new: Shape) -> Option<Rect> {
        let i = self.index_of(id)?;
        let dirty = rect_union(old.bbox(), new.bbox());
        self.shapes[i].shape = new.clone();
        self.undo.push(Command::Transform { id, old, new });
        self.redo.clear();
        Some(dirty)
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
            Command::Transform { id, old, new } => {
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
            Command::Delete { index, ann } => {
                let i = (*index).min(self.shapes.len().saturating_sub(1));
                if !self.shapes.is_empty() {
                    self.shapes.remove(i);
                }
                ann.shape.bbox()
            }
            Command::Transform { id, old, new } => {
                if let Some(i) = self.index_of(*id) {
                    self.shapes[i].shape = new.clone();
                }
                rect_union(old.bbox(), new.bbox())
            }
        }
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

    use super::super::geometry::Pt;

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
}

//! Epic derivation: an epic is a task with `kind = "epic"` that groups other
//! tasks through their `epic_id`. Progress is always derived from the
//! children — jay stores no counter that could drift from the data.

use crate::model::{Task, TaskKind, TaskStatus};

/// Derived progress of one epic, counted over its direct children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpicProgress {
    /// Number of tasks whose `epic_id` is this epic.
    pub total: usize,
    /// Children in the successful terminal state.
    pub closed: usize,
    /// Children explicitly abandoned (kept separate from closed).
    pub cancelled: usize,
}

impl EpicProgress {
    /// Children that are still open, started or in review.
    pub fn open(self) -> usize {
        self.total - self.closed - self.cancelled
    }

    /// True when the epic has at least one child and none of them is still
    /// open: every child is closed or cancelled. A cancelled child was
    /// explicitly descoped, so it does not hold the epic open (the same
    /// treatment a cancelled dependency gets for readiness). An epic with no
    /// children is never complete (nothing to derive from), so it is not
    /// advanced automatically.
    pub fn is_complete(self) -> bool {
        self.total > 0 && self.open() == 0
    }

    /// True when the epic has no children at all.
    pub fn has_no_children(self) -> bool {
        self.total == 0
    }

    /// Fraction of children closed, between 0.0 and 1.0 (0.0 with no children).
    pub fn ratio(self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.closed as f64 / self.total as f64
        }
    }
}

/// The direct children of `epic_id`, in id order.
pub fn children(all: &[Task], epic_id: i64) -> Vec<&Task> {
    let mut out: Vec<&Task> = all.iter().filter(|t| t.epic_id == Some(epic_id)).collect();
    out.sort_by_key(|t| t.id);
    out
}

/// Derived progress of the epic with `epic_id`.
pub fn progress(all: &[Task], epic_id: i64) -> EpicProgress {
    let children = children(all, epic_id);
    EpicProgress {
        total: children.len(),
        closed: children
            .iter()
            .filter(|t| t.status == TaskStatus::Closed)
            .count(),
        cancelled: children
            .iter()
            .filter(|t| t.status == TaskStatus::Cancelled)
            .count(),
    }
}

/// Every epic in the project, in id order.
pub fn list(all: &[Task]) -> Vec<&Task> {
    let mut out: Vec<&Task> = all.iter().filter(|t| t.kind == TaskKind::Epic).collect();
    out.sort_by_key(|t| t.id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: i64, epic: Option<i64>, status: TaskStatus) -> Task {
        let mut t = Task::new(id, format!("t{id}"), String::new());
        t.epic_id = epic;
        t.status = status;
        t
    }

    #[test]
    fn progress_counts_children_and_closed() {
        let epic = {
            let mut e = Task::new(1, "epic".into(), String::new());
            e.kind = TaskKind::Epic;
            e
        };
        let all = vec![
            epic,
            task(2, Some(1), TaskStatus::Closed),
            task(3, Some(1), TaskStatus::Started),
            task(4, Some(1), TaskStatus::Cancelled),
            task(5, None, TaskStatus::Open),
            task(6, None, TaskStatus::Closed),
        ];
        let p = progress(&all, 1);
        assert_eq!(p.total, 3);
        assert_eq!(p.closed, 1);
        assert_eq!(p.cancelled, 1);
        assert!(!p.is_complete());
        assert!(!p.has_no_children());
        assert!((p.ratio() - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn progress_is_complete_when_no_child_is_still_open() {
        let empty = progress(&[], 1);
        assert!(empty.has_no_children());
        assert!(!empty.is_complete(), "no children is not complete");
        assert_eq!(empty.ratio(), 0.0);

        let epic = {
            let mut e = Task::new(1, "epic".into(), String::new());
            e.kind = TaskKind::Epic;
            e
        };
        let all = vec![
            epic,
            task(2, Some(1), TaskStatus::Closed),
            task(3, Some(1), TaskStatus::Closed),
        ];
        let p = progress(&all, 1);
        assert!(p.is_complete());
        assert_eq!(p.open(), 0);
        assert_eq!(p.ratio(), 1.0);
    }

    #[test]
    fn a_cancelled_child_is_resolved_and_does_not_hold_the_epic_open() {
        let epic = {
            let mut e = Task::new(1, "epic".into(), String::new());
            e.kind = TaskKind::Epic;
            e
        };
        let all = vec![
            epic,
            task(2, Some(1), TaskStatus::Closed),
            task(3, Some(1), TaskStatus::Cancelled),
        ];
        let p = progress(&all, 1);
        assert_eq!(p.closed, 1);
        assert_eq!(p.cancelled, 1);
        assert_eq!(p.open(), 0);
        assert!(p.is_complete(), "descoped children do not block the epic");
    }

    #[test]
    fn children_are_ordered_and_list_returns_only_epics() {
        let mut epic = Task::new(1, "epic".into(), String::new());
        epic.kind = TaskKind::Epic;
        let all = vec![
            task(3, Some(1), TaskStatus::Open),
            epic,
            task(2, Some(1), TaskStatus::Open),
            task(4, None, TaskStatus::Open),
        ];
        let ids: Vec<i64> = children(&all, 1).iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 3]);
        let epics: Vec<i64> = list(&all).iter().map(|t| t.id).collect();
        assert_eq!(epics, vec![1]);
    }

    #[test]
    fn a_task_pointing_at_a_different_epic_is_not_a_child() {
        let mut epic = Task::new(1, "epic".into(), String::new());
        epic.kind = TaskKind::Epic;
        epic.epic_id = Some(9);
        let all = vec![epic];
        assert!(children(&all, 1).is_empty());
    }
}

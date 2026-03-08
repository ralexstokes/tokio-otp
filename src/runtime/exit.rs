use crate::{
    child::{BoxError, ChildResult},
    event::ExitStatusView,
};

#[derive(Debug)]
pub(crate) enum ExitStatus {
    Completed,
    Failed(BoxError),
    Panicked,
    Aborted,
}

impl ExitStatus {
    pub(crate) fn from_child_result(result: ChildResult) -> Self {
        match result {
            ChildResult::Completed => Self::Completed,
            ChildResult::Failed(err) => Self::Failed(err),
        }
    }

    pub(crate) fn is_failure(&self) -> bool {
        !matches!(self, Self::Completed)
    }

    pub(crate) fn view(&self) -> ExitStatusView {
        match self {
            Self::Completed => ExitStatusView::Completed,
            Self::Failed(err) => ExitStatusView::Failed(err.to_string()),
            Self::Panicked => ExitStatusView::Panicked,
            Self::Aborted => ExitStatusView::Aborted,
        }
    }
}

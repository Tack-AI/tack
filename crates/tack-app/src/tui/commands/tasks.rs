//! `/tasks` — open the live background task & agent control panel.

use super::super::TuiApp;
use super::TaskDialog;

impl TuiApp {
    pub(crate) fn command_tasks(&mut self) {
        self.dialog = Some(super::Dialog::Tasks(TaskDialog::new(
            self.background_tasks.clone(),
            self.theme,
        )));
    }
}

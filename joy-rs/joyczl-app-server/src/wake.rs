//! Both event sources re-enter the same task recovery and accounting driver.
use crate::Server;
use joyczl_state::{
    external_waits::{ExternalWaits, InputWait},
    waits::{JobWait, JobWaits},
};

pub(crate) enum Wake {
    Jobs(JobWait),
    Input(InputWait),
}
impl Wake {
    pub(crate) fn checkpoint_status(&self) -> &'static str {
        match self {
            Self::Jobs(_) => "waiting_jobs",
            Self::Input(_) => "waiting_input",
        }
    }
    pub(crate) fn task_id(&self) -> &str {
        match self {
            Self::Jobs(wait) => &wait.task_id,
            Self::Input(wait) => &wait.task_id,
        }
    }
    pub(crate) async fn claim(&self, server: &Server, messages: &str) -> anyhow::Result<bool> {
        match self {
            Self::Jobs(wait) => {
                JobWaits::new(server.pool.clone())
                    .claim(wait, messages)
                    .await
            }
            Self::Input(wait) => {
                ExternalWaits::new(server.pool.clone())
                    .claim(wait, messages)
                    .await
            }
        }
    }
}

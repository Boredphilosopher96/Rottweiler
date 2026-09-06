//! Proxy shutdown remains owned when a process settlement caller disappears.

use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use rw_ext::PluginProcessError;
use rw_tools::SupervisedEgressProxy;
use std::sync::Mutex;

type Completion = Shared<BoxFuture<'static, Result<(), PluginProcessError>>>;
enum State {
    Live(Option<SupervisedEgressProxy>),
    Stopping(Completion),
}
pub(super) struct PluginProxy(Mutex<State>);
impl PluginProxy {
    pub(super) fn new(proxy: Option<SupervisedEgressProxy>) -> Self {
        Self(Mutex::new(State::Live(proxy)))
    }
    pub(super) async fn settle(&self) -> Result<(), PluginProcessError> {
        let completion = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*state {
                State::Stopping(completion) => completion.clone(),
                State::Live(_) => {
                    let (send, receive) = tokio::sync::oneshot::channel();
                    let completion = async move {
                        receive.await.map_err(|_| PluginProcessError {
                            message: "plugin proxy owner exited without settlement proof"
                                .to_owned(),
                        })?
                    }
                    .boxed()
                    .shared();
                    let State::Live(proxy) =
                        std::mem::replace(&mut *state, State::Stopping(completion.clone()))
                    else {
                        unreachable!()
                    };
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || drop(proxy))
                            .await
                            .map_err(|error| PluginProcessError {
                                message: format!("plugin proxy cleanup failed: {error}"),
                            });
                        let _ = send.send(result);
                    });
                    completion
                }
            }
        };
        completion.await
    }
}

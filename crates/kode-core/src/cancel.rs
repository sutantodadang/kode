pub use tokio_util::sync::CancellationToken;

/// Spawns a task that cancels `token` on Ctrl-C.
pub fn cancel_on_ctrl_c(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            token.cancel();
        }
    });
}

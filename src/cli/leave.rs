pub async fn run() -> anyhow::Result<()> {
  crate::client_owner::dispatch(crate::client_owner::Command::Leave {}).await
}

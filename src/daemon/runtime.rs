//! Native and Edge resources owned only by the client_owner supervisor.
use super::loop_runner::{LocalPath, LogBuffer, PeerInfo, SyncState};
use super::network_events::{NetworkEvents, PendingEvent};
use super::peer_paths;
use crate::{
  client_owner::{
    supervisor::{Driver, Work},
    Command,
  },
  config::{self, AppConfig},
  managers::{cert::CertManager, WgManager},
};
use anyhow::{Context, Result};
use std::{
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::{Duration, Instant},
};
use tokio::sync::{Mutex, Notify};
use yat_network::wg::sync_controller::PathRole;
use yat_network::WgSyncController;
use yat_transport::WGNetworkRuntimeSnapshotData;
use yat_wireguard::types::RuntimeStats;

#[derive(Clone)]
pub struct Shared {
  pub sync: Arc<Mutex<SyncState>>,
  pub logs: Arc<Mutex<LogBuffer>>,
  pub config: Arc<Mutex<Option<Arc<AppConfig>>>>,
  pub cert_manager: Arc<CertManager>,
}
impl Shared {
  pub fn new() -> Self {
    Self {
      sync: Arc::new(Mutex::new(SyncState::empty())),
      logs: Arc::new(Mutex::new(LogBuffer::new())),
      config: Arc::new(Mutex::new(config::load_config().ok().map(Arc::new))),
      cert_manager: Arc::new(CertManager::new(&config::config_dir())),
    }
  }
}

pub struct Runtime {
  wakeup: Arc<Notify>,
  events: Option<Arc<NetworkEvents>>,
  shared: Shared,
  manager: Option<WgManager>,
  controller: Option<Arc<WgSyncController>>,
  gather: Option<crate::gather::Subscription>,
  network_id: String,
  storage_dir: String,
  local_public_key: String,
  relay_public_keys: Vec<String>,
  enabled: bool,
  start_on_import: bool,
  needs_drain: bool,
  // Preserve explicit Leave intent across failed drain/retry. Stop/reconnect
  // alone must never erase the pair's durable setting.
  pending_mode_removal: Option<String>,
  next_attempt: Instant,
  failures: u32,
  last_sync: Instant,
  last_mode_sync: Instant,
  path_retry_after: Instant,
  last_runtime_sync: Instant,
}

fn new_controller(storage_dir: String) -> Arc<WgSyncController> {
  Arc::new(
    WgSyncController::new(
      yat_network::wg::engine::create_engine(None),
      storage_dir,
      false,
    )
    .with_path_role(PathRole::Responder),
  )
}

impl Runtime {
  pub fn new(shared: Option<Shared>) -> Self {
    let start_on_import = shared.is_some();
    let storage_dir = config::config_dir()
      .join("wireguard")
      .to_string_lossy()
      .into_owned();
    Self {
      wakeup: Arc::new(Notify::new()),
      events: None,
      shared: shared.unwrap_or_else(Shared::new),
      manager: None,
      // Also owns cleanup of disk-only leftovers, under the same OS lock.
      controller: Some(new_controller(storage_dir.clone())),
      gather: None,
      network_id: String::new(),
      storage_dir,
      local_public_key: String::new(),
      relay_public_keys: Vec::new(),
      enabled: false,
      start_on_import,
      needs_drain: true,
      pending_mode_removal: None,
      next_attempt: Instant::now(),
      failures: 0,
      last_sync: Instant::now(),
      last_mode_sync: Instant::now(),
      path_retry_after: Instant::now(),
      last_runtime_sync: Instant::now(),
    }
  }

  async fn log(&self, level: &str, message: String) {
    self.shared.logs.lock().await.push(level, message);
  }

  fn check_running(stopping: &AtomicBool) -> Result<()> {
    anyhow::ensure!(!stopping.load(Ordering::Acquire), "Client is shutting down");
    Ok(())
  }

  async fn stop_local(&mut self, leave: bool) -> Result<()> {
    self.needs_drain = true;
    if let Some(controller) = self.controller.as_ref() {
      controller.begin_shutdown();
    }
    if let Some(events) = &self.events {
      events.close();
    }
    if let Some(manager) = self.manager.as_mut() {
      if let Some(edge) = manager.edge_client() {
        edge.unsubscribe_wg_network_events().await?;
        edge.unsubscribe_wg_p2p_events().await?;
      }
      // Close control/heartbeat before waiting for native operations. Edge can
      // mark the device offline even when a later native drain must retry.
      manager.disconnect_owned(leave).await?;
    }
    if let Some(gather) = self.gather.as_mut() {
      gather.stop().await?;
    }
    self.gather = None;
    self.events = None;
    if let Some(controller) = self.controller.as_ref() {
      // Retain this exact instance on failure. No replacement is legal yet.
      if let Some(network) = &self.pending_mode_removal {
        controller.teardown_network_for_removal(network).await?;
      }
      controller.shutdown_networks().await?;
    }
    self.pending_mode_removal = None;
    self.manager = None;
    self.controller = None;
    self.network_id.clear();
    self.local_public_key.clear();
    self.relay_public_keys.clear();
    std::fs::remove_file(config::config_dir().join("state.json")).or_else(|error| {
      if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
      } else {
        Err(error)
      }
    })?;
    *self.shared.sync.lock().await = SyncState::empty();
    self.needs_drain = false;
    Ok(())
  }

  pub async fn cleanup_orphan(&mut self) -> Result<()> {
    let previous = crate::managers::wg::load_state();
    self.stop_local(false).await?;
    if let Some(state) = previous {
      // Remote leave is best effort and cannot race another local writer:
      // the caller still holds the same process lock.
      if let Ok(mut manager) = WgManager::new() {
        manager
          .leave_saved_member(&state.network_id, &state.member_id)
          .await;
      }
    }
    Ok(())
  }

  async fn connect(&mut self, stopping: &AtomicBool) -> Result<()> {
    self.stop_local(false).await?;
    Self::check_running(stopping)?;
    anyhow::ensure!(
      !config::config_dir().join("import.pending").exists(),
      "Previous import incomplete; re-import share package"
    );
    let cfg = config::load_config()?;
    *self.shared.config.lock().await = Some(Arc::new(cfg.clone()));
    let certs = &self.shared.cert_manager;
    if !certs.has_client_cert() {
      let invite = cfg
        .invite_token()
        .context("No client certificate or invite token; import share.json")?;
      certs
        .bootstrap_certificate(&cfg.edge, &cfg.identity.device_id, &invite)
        .await?;
    }
    Self::check_running(stopping)?;
    self.manager = Some(WgManager::from_config(cfg)?);
    self.needs_drain = true;
    let ctx = self.manager.as_mut().unwrap().connect_edge().await?;
    Self::check_running(stopping)?;
    let identity = tokio::task::spawn_blocking({
      let storage_dir = ctx.storage_dir.clone();
      move || {
        yat_wireguard::identity::get_or_create_local_identity(
          yat_wireguard::types::LocalIdentityParams {
            storage_dir,
            preferred_device_id: None,
            hostname: None,
          },
        )
      }
    })
    .await??;
    Self::check_running(stopping)?;
    let controller = new_controller(ctx.storage_dir.clone());
    self.local_public_key = identity.public_key;
    controller.set_private_key(identity.private_key);
    self.controller = Some(controller.clone());
    self.network_id = ctx.network_id.clone();
    self.storage_dir = ctx.storage_dir.clone();
    let edge = ctx.edge.clone();
    let events = Arc::new(NetworkEvents::new(
      self.network_id.clone(),
      self.wakeup.clone(),
    ));
    self.events = Some(events.clone());
    edge
      .subscribe_wg_network_events(move |event| events.observe(&event.network_id, event.action))
      .await?;
    let client_id = ctx.client_id.clone();
    let self_address = ctx.self_address.clone();
    let mut snapshot = ctx.snapshot.clone();
    let status = self
      .manager
      .as_mut()
      .unwrap()
      .apply_wg_via_controller(ctx, controller.clone())
      .await?;
    Self::check_running(stopping)?;
    self.gather = Some(
      crate::gather::subscribe_p2p_events(
        edge.clone(),
        client_id,
        self.storage_dir.clone(),
        self.network_id.clone(),
        self_address,
      )
      .await?,
    );
    {
      let mut state = self.shared.sync.lock().await;
      state.connected = true;
      state.network_id = status.network_id;
      state.member_id = status.member_id;
      state.self_address = status.self_address;
      state.interface_name = status.interface_name;
    }
    // Attach immediately after the initial native apply. Progress events that
    // arrive during assembly remain latched for the next supervisor turn.
    self.path_retry_after = Instant::now();
    match controller
      .reconcile_path_control_snapshot(edge, &self.network_id)
      .await
    {
      Ok(Some(fresh)) => snapshot = fresh,
      Ok(None) => (),
      Err(error) => {
        tracing::warn!("WG responder assembly deferred: {error:#}");
        self.path_retry_after = Instant::now() + Duration::from_secs(5);
      }
    }
    Self::check_running(stopping)?;
    self.update_snapshot(&snapshot).await;
    self.needs_drain = false;
    self.last_sync = Instant::now();
    self.last_mode_sync = Instant::now();
    self.failures = 0;
    self
      .log("INFO", "Connected; initial native apply verified".into())
      .await;
    Ok(())
  }

  async fn start(&mut self, stopping: &AtomicBool) -> Result<()> {
    self.enabled = true;
    let result = self.connect(stopping).await;
    if let Err(error) = result {
      // Partial startup owns its resources until this exact drain succeeds.
      let cleanup = self
        .stop_local(false)
        .await
        .context("Failed startup cleanup; replacement blocked");
      self.schedule_retry().await;
      cleanup?;
      return Err(error);
    }
    Ok(())
  }

  async fn schedule_retry(&mut self) {
    self.failures += 1;
    let delay = if self.failures >= 10 {
      30 * 60
    } else {
      (1u64 << self.failures.min(5)).min(30)
    };
    self.next_attempt = Instant::now() + Duration::from_secs(delay);
    let next = chrono::Local::now() + chrono::Duration::seconds(delay as i64);
    let mut state = self.shared.sync.lock().await;
    state.connected = false;
    state.reconnect_at = Some(next.to_rfc3339());
  }

  async fn update_snapshot(&mut self, snapshot: &WGNetworkRuntimeSnapshotData) {
    use crate::managers::wg::is_valid_wireguard_key;
    self.relay_public_keys = snapshot
      .relay_node
      .iter()
      .map(|relay| relay.public_key.clone())
      .chain(
        snapshot
          .members
          .iter()
          .filter(|m| m.is_relay_node)
          .map(|m| m.public_key.clone()),
      )
      .collect();
    let mut peers = snapshot
      .members
      .iter()
      .filter(|m| !m.is_current_device && is_valid_wireguard_key(&m.public_key))
      .map(|m| PeerInfo {
        hostname: m.hostname.clone(),
        assigned_ip: m.assigned_ip.clone(),
        public_key: m.public_key.clone(),
        local_path: LocalPath::Unknown,
        endpoint: String::new(),
        presence_status: m.presence_status,
        last_handshake_at: None,
      })
      .collect::<Vec<_>>();
    let stats = self.read_runtime_stats().await;
    peer_paths::update(&mut peers, stats.as_ref(), &self.relay_public_keys);
    let mut state = self.shared.sync.lock().await;
    state.network_name = snapshot.network.name.clone();
    state.subnet = snapshot.network.subnet.clone();
    state.peer_count = peers.len() as u32;
    state.peers = peers;
    state.last_sync_at = Some(chrono::Local::now().to_rfc3339());
    if let Some(member) = snapshot.members.iter().find(|m| m.is_current_device) {
      state.hostname = member.hostname.clone();
      state.self_address = member.assigned_ip.clone();
    }
  }

  async fn read_runtime_stats(&mut self) -> Option<RuntimeStats> {
    let result: Result<Option<RuntimeStats>> = async {
      Ok(
        tokio::task::spawn_blocking({
          let storage_dir = self.storage_dir.clone();
          let network_id = self.network_id.clone();
          move || {
            yat_wireguard::stats::get_runtime_stats(yat_wireguard::types::RuntimeStatsParams {
              storage_dir,
              network_id,
            })
          }
        })
        .await??,
      )
    }
    .await;
    self.last_runtime_sync = Instant::now();
    match result {
      Ok(Some(stats)) => {
        let interface = self.shared.sync.lock().await.interface_name.clone();
        if stats.network_id == self.network_id
          && stats.local_public_key == self.local_public_key
          && stats.resolved_interface_name == interface
        {
          Some(stats)
        } else {
          tracing::warn!("Local path display deferred: runtime identity mismatch");
          None
        }
      }
      Ok(None) => None,
      Err(error) => {
        tracing::warn!("Local path display refresh failed: {error:#}");
        None
      }
    }
  }

  async fn refresh_peer_paths(&mut self) {
    let stats = self.read_runtime_stats().await;
    let mut state = self.shared.sync.lock().await;
    peer_paths::update(&mut state.peers, stats.as_ref(), &self.relay_public_keys);
  }

  async fn refresh(&mut self, stopping: &AtomicBool) -> Result<()> {
    Self::check_running(stopping)?;
    let event = self
      .events
      .as_ref()
      .map(|events| events.take())
      .unwrap_or(PendingEvent::None);
    if event == PendingEvent::Deleted {
      self.enabled = false;
      self.pending_mode_removal = Some(self.network_id.clone());
      self.stop_local(false).await?;
      self
        .log("INFO", "Network deleted; responder stopped".into())
        .await;
      return Ok(());
    }
    let edge = self
      .manager
      .as_ref()
      .and_then(WgManager::edge_client)
      .context("No active connection")?;
    if !edge.is_connected() {
      return self.start(stopping).await;
    }
    let controller = self.controller.as_ref().context("No runtime controller")?;
    let path_controlled = controller.is_path_controlled(&self.network_id).await;
    let mut refreshed = None;
    if event == PendingEvent::Refresh && Instant::now() < self.path_retry_after {
      if let Some(events) = &self.events {
        events.defer_refresh();
      }
    }
    // Events advance barriers immediately; the timer only repairs missed
    // notifications/attachments. A failed query imposes the same retry bound
    // even when unrelated pair events continue arriving.
    if (path_controlled || controller.supports_path_activation())
      && Instant::now() >= self.path_retry_after
      && (event == PendingEvent::Refresh || self.last_mode_sync.elapsed() >= Duration::from_secs(5))
    {
      self.last_mode_sync = Instant::now();
      match controller
        .reconcile_path_control_snapshot(edge.clone(), &self.network_id)
        .await
      {
        Ok(snapshot) => refreshed = snapshot,
        Err(error) => {
          tracing::warn!("WG responder refresh deferred: {error:#}");
          self.path_retry_after = Instant::now() + Duration::from_secs(5);
          if let Some(events) = &self.events {
            events.defer_refresh();
          }
        }
      }
      Self::check_running(stopping)?;
    }
    if refreshed.is_none() && self.last_sync.elapsed() < Duration::from_secs(30) {
      return Ok(());
    }
    // Reuse the authenticated coordination read. Products without coordination
    // retain their ordinary snapshot fallback, bounded even on query failure.
    self.last_sync = Instant::now();
    let snapshot = match refreshed {
      Some(snapshot) => snapshot,
      None => {
        let response = edge.request_wg_get_network(&self.network_id).await?;
        anyhow::ensure!(
          response.success,
          "Snapshot rejected: {}",
          response.error_message
        );
        response.snapshot.context("Missing network snapshot")?
      }
    };
    Self::check_running(stopping)?;

    // If the applied state file is missing (e.g. after a partial teardown or
    // failed reapply during a previous peer-key upgrade), legacy mode can ask
    // the controller to rebuild it. A coordinated network keeps its sticky
    // ownership and leaves recovery to its authenticated executor instead.
    let applied_public_key = match yat_wireguard::applied::get_applied_runtime_state(
      yat_wireguard::types::AppliedRuntimeStateParams {
        storage_dir: self.storage_dir.clone(),
        network_id: self.network_id.clone(),
      },
    )? {
      Some(state) => Some(state.local_public_key),
      None => {
        if path_controlled {
          tracing::warn!(
            "[refresh] applied runtime state missing for coordinated network {}; legacy apply stays disabled",
            self.network_id
          );
        } else {
          tracing::warn!(
            "[refresh] applied runtime state missing for network {}, will force full reapply",
            self.network_id
          );
        }
        None
      }
    };

    let member_id = self.shared.sync.lock().await.member_id.clone();

    // Only validate identity when we have an applied state to compare against.
    // When applied state is missing, skip validation and let the sync controller
    // rebuild everything from the snapshot.
    if let Some(ref applied_key) = applied_public_key {
      crate::managers::wg::validate_snapshot_identity(
        &snapshot,
        &self.network_id,
        &member_id,
        applied_key,
      )?;
    }

    // S1: snapshot ingestion always runs regardless of path_owned.
    // Native apply is gated inside reconcile_snapshot_locked.
    controller
      .handle_typed_snapshot(&self.network_id, &snapshot)
      .await?;
    self.update_snapshot(&snapshot).await;
    Ok(())
  }
}

impl Driver for Runtime {
  fn wakeup(&self) -> Option<Arc<Notify>> {
    Some(self.wakeup.clone())
  }
  fn execute<'a>(&'a mut self, command: Command, stopping: &'a AtomicBool) -> Work<'a> {
    Box::pin(async move {
      Self::check_running(stopping)?;
      match command {
        Command::Start {} => {
          if !self.needs_drain
            && self
              .manager
              .as_ref()
              .and_then(WgManager::edge_client)
              .is_some_and(|edge| edge.is_connected())
          {
            return Ok(());
          }
          self.start(stopping).await
        }
        Command::Leave {} => {
          self.enabled = false;
          let network = if !self.network_id.is_empty() {
            Some(self.network_id.clone())
          } else {
            crate::managers::wg::load_state()
              .map(|state| state.network_id)
              .or_else(|| config::load_config().ok().and_then(|cfg| cfg.network_id()))
          };
          if let Some(network) = network {
            self.pending_mode_removal = Some(network);
            if self.controller.is_none() {
              // The controller is only dropped after successful native drain;
              // needs_drain may still reflect a failed state.json cleanup.
              // Reuse this supervisor even for a disk-only explicit removal.
              self.controller = Some(new_controller(self.storage_dir.clone()));
            }
          }
          if self.manager.is_none() {
            self.cleanup_orphan().await
          } else {
            self.stop_local(true).await
          }
        }
        Command::Import { content } => {
          crate::cli::import::validate_content(&content)?;
          self.enabled = false;
          self.stop_local(false).await?;
          Self::check_running(stopping)?;
          crate::cli::import::import_content(&content)?;
          *self.shared.config.lock().await = Some(Arc::new(config::load_config()?));
          if self.start_on_import {
            self.start(stopping).await
          } else {
            Ok(())
          }
        }
        Command::UpdateAddress { new_ip } => {
          anyhow::ensure!(!self.needs_drain, "Runtime is draining");
          let state = self.shared.sync.lock().await;
          let member_id = state.member_id.clone();
          drop(state);
          let edge = self
            .manager
            .as_ref()
            .and_then(WgManager::edge_client)
            .context("Not connected")?;
          let response = edge
            .update_wg_member(&self.network_id, &member_id, &new_ip, "")
            .await?;
          anyhow::ensure!(response.success, "Edge rejected address update");
          Self::check_running(stopping)?;
          self.shared.sync.lock().await.self_address = new_ip;
          self.last_sync = Instant::now() - Duration::from_secs(30);
          self.refresh(stopping).await
        }
      }
    })
  }
  fn tick<'a>(&'a mut self, stopping: &'a AtomicBool) -> Work<'a> {
    Box::pin(async move {
      if self.needs_drain {
        self.stop_local(false).await?;
      }
      if !self.enabled {
        return Ok(());
      }
      if self.manager.is_none() {
        if Instant::now() >= self.next_attempt {
          self.start(stopping).await?;
        }
      } else {
        let result = self.refresh(stopping).await;
        // Native display reads share this supervisor and remain available when
        // an Edge query fails. They never send control requests or change paths.
        if !stopping.load(Ordering::Acquire)
          && !self.needs_drain
          && self.manager.is_some()
          && self.last_runtime_sync.elapsed() >= Duration::from_secs(5)
        {
          self.refresh_peer_paths().await;
        }
        result?;
      }
      Ok(())
    })
  }
  fn stop(&mut self) -> Work<'_> {
    Box::pin(async move {
      self.enabled = false;
      self.stop_local(false).await
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_linux_controller_starts_with_responder_permissions() {
    let controller = new_controller("/unused/linux-responder".into());
    assert_eq!(controller.path_role(), PathRole::Responder);
    assert!(!controller.is_shutting_down());
  }
}

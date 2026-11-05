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
use yat_transport::{TransportError, WGNetworkRuntimeSnapshotData};
use yat_wireguard::types::RuntimeStats;

// Only a transport failure may replace a live control session automatically.
// Native IO and Edge authorization/coordination rejections are not evidence
// that authentication must be rebuilt.
fn control_connection_failed(error: &anyhow::Error) -> bool {
  matches!(
    error.downcast_ref::<TransportError>(),
    Some(
      TransportError::Io(_)
        | TransportError::Tls(_)
        | TransportError::Connection(_)
        | TransportError::Timeout
        | TransportError::NotConnected
    )
  )
}

fn ordinary_snapshot_reconcile_due(event: PendingEvent, last_sync: Duration) -> bool {
  event == PendingEvent::Refresh || last_sync >= Duration::from_secs(30)
}

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
        if let Some(controller)=&self.controller {
          if let Ok((edge_id,session_id))=edge.authenticated_event_source().await {
            controller.events.retire_source(&yat_network::events::EventSource {edge_id,session_id});
          }
        }
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
    let mut state = self.shared.sync.lock().await;
    // A delayed native drain can finish on a later supervisor tick. Keep the
    // already scheduled retry visible until success or explicit Leave/Import.
    let reconnect_at = if self.enabled {
      state.reconnect_at.take()
    } else {
      None
    };
    *state = SyncState::empty();
    state.reconnect_at = reconnect_at;
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
    let source=yat_network::edge::event_adapter::attach(&edge,controller.events.clone()).await?;
    let events=Arc::new(NetworkEvents::new(controller.events.clone(),source.edge_id,self.network_id.clone(),self.wakeup.clone()));
    self.events=Some(events);
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
        controller.events.clone(),
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
      state.reconnect_at = None;
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
      Ok(Some(fresh)) => {
        let path_controlled = controller.is_path_controlled(&self.network_id).await;
        tracing::info!(
          network = %self.network_id,
          path_controlled,
          authority_epoch = %fresh.path_authority_epoch,
          "WG responder initial bootstrap completed"
        );
        snapshot = fresh;
      }
      Ok(None) => (),
      Err(error) => {
        if control_connection_failed(&error) {
          return Err(error);
        }
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
    self.failures = self.failures.saturating_add(1);
    // Keep recovering after a long outage. The old 30-minute cooldown after
    // ten failures could leave a restarted Edge apparently disconnected.
    let delay = (1u64 << self.failures.min(5)).min(30);
    self.next_attempt = Instant::now() + Duration::from_secs(delay);
    let next = chrono::Local::now() + chrono::Duration::seconds(delay as i64);
    let mut state = self.shared.sync.lock().await;
    state.connected = false;
    state.reconnect_at = Some(next.to_rfc3339());
    drop(state);
    tracing::warn!(
      attempt = self.failures,
      retry_in_secs = delay,
      "Edge connection retry scheduled"
    );
    self
      .log(
        "WARN",
        format!("Edge reconnect attempt failed; retrying in {delay}s"),
      )
      .await;
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
        link_health:None,
      })
      .collect::<Vec<_>>();
    let stats = self.read_runtime_stats().await;
    let health=if let Some(controller)=&self.controller {controller.peer_link_health(&self.network_id).await}else{vec![]};
    peer_paths::update_with_health(&mut peers, stats.as_ref(), &self.relay_public_keys,&health);
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
    let health=if let Some(controller)=&self.controller {controller.peer_link_health(&self.network_id).await}else{vec![]};
    let mut state = self.shared.sync.lock().await;
    peer_paths::update_with_health(&mut state.peers, stats.as_ref(), &self.relay_public_keys,&health);
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
      return Err(TransportError::NotConnected.into());
    }
    let controller = self.controller.as_ref().context("No runtime controller")?;
    let path_controlled = controller.is_path_controlled(&self.network_id).await;
    let mut refreshed = None;
    if event == PendingEvent::Refresh && Instant::now() < self.path_retry_after {
      if let Some(events) = &self.events {
        events.defer_refresh();
      }
    }
    if event==PendingEvent::None && edge.supports_reliable_runtime_push()
      && controller.tick_path_control(&self.network_id).await? {return Ok(());}
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
          if control_connection_failed(&error) {
            return Err(error);
          }
          tracing::warn!("WG responder refresh deferred: {error:#}");
          self.path_retry_after = Instant::now() + Duration::from_secs(5);
          if let Some(events) = &self.events {
            events.defer_refresh();
          }
        }
      }
      Self::check_running(stopping)?;
    }
    if let Some(snapshot) = refreshed.as_ref() {
      if controller.is_path_controlled(&self.network_id).await {
        // The authenticated path refresh already ingested topology after
        // advancing READY/COMMIT. Do not repeat stats/sync work before the next
        // queued barrier; only project the current local path for the Web UI.
        self.last_sync = Instant::now();
        self.update_snapshot(snapshot).await;
        return Ok(());
      }
      // A 5s runtime/barrier read is not a reason to run the legacy topology
      // sync pipeline on every tick. Keep the UI fresh, but reconcile ordinary
      // WG state at the existing 30s fallback cadence unless an event drove
      // this refresh. Linux disables the legacy timer-based full re-apply; real
      // changes are still applied by the snapshot reconciler.
      if !ordinary_snapshot_reconcile_due(event, self.last_sync.elapsed()) {
        self.update_snapshot(snapshot).await;
        return Ok(());
      }
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
        let (edge_id,session_id)=edge.authenticated_event_source().await?;
        controller.events.ensure_current(&yat_network::events::EventSource {edge_id,session_id},&edge,&self.network_id,true).await?
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
        // Explicit recovery must rebuild authentication and the responder even
        // if the transport is connected. Normal Start remains idempotent.
        // start() drains the original owner and preserves saved pair modes.
        Command::Reconnect {} => self.start(stopping).await,
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
        Command::Pair { edge, token } => {
          self.enabled = false;
          self.stop_local(false).await?;
          Self::check_running(stopping)?;
          crate::cli::pair::pair_owned(&edge, &token).await?;
          *self.shared.config.lock().await = Some(Arc::new(config::load_config()?));
          // Pairing does not join a WireGuard network or auto-start it.
          Ok(())
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
        if let Err(error) = &result {
          if !stopping.load(Ordering::Acquire) && control_connection_failed(error) {
            tracing::warn!(network = %self.network_id, error = %error, "Edge control connection lost; reconnecting");
            self
              .log(
                "WARN",
                format!("Edge control connection lost; reconnecting: {error:#}"),
              )
              .await;
            // Reuse the original owner: drain the old session/controller,
            // authenticate, rejoin, and install fresh event subscriptions.
            // start() schedules exactly one retry if this attempt fails.
            return self.start(stopping).await;
          }
        }
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

  fn runtime() -> Runtime {
    Runtime::new(Some(Shared {
      sync: Arc::new(Mutex::new(SyncState::empty())),
      logs: Arc::new(Mutex::new(LogBuffer::new())),
      config: Arc::new(Mutex::new(None)),
      cert_manager: Arc::new(CertManager::new(std::path::Path::new(
        "/unused/reconnect-test",
      ))),
    }))
  }

  #[tokio::test]
  async fn prolonged_edge_outage_keeps_retrying_with_a_thirty_second_cap() {
    let mut runtime = runtime();
    runtime.shared.sync.lock().await.connected = true;
    for attempt in 1..=25 {
      let before = Instant::now();
      runtime.schedule_retry().await;
      let delay = runtime.next_attempt.duration_since(before);
      assert!(
        delay <= Duration::from_secs(31),
        "attempt {attempt}: {delay:?}"
      );
      assert!(delay >= Duration::from_secs(2));
      assert_eq!(runtime.failures, attempt);
      let state = runtime.shared.sync.lock().await;
      assert!(!state.connected);
      let deadline =
        chrono::DateTime::parse_from_rfc3339(state.reconnect_at.as_ref().unwrap()).unwrap();
      let remaining = deadline.signed_duration_since(chrono::Local::now());
      assert!(remaining.num_seconds() <= 30);
      assert!(remaining.num_seconds() >= 0);
    }
    runtime.failures = u32::MAX;
    runtime.schedule_retry().await;
    assert_eq!(runtime.failures, u32::MAX);
    assert!(runtime.next_attempt.duration_since(Instant::now()) <= Duration::from_secs(30));
  }

  #[tokio::test]
  async fn queued_wakeups_do_not_bypass_connection_retry_deadline() {
    let mut runtime = runtime();
    runtime.enabled = true;
    runtime.needs_drain = false;
    runtime.schedule_retry().await;
    // Keep this gate test away from real config/native startup even on a
    // heavily paused test runner. Actual retry intervals are covered above.
    runtime.next_attempt = Instant::now() + Duration::from_secs(3600);
    let deadline = runtime.next_attempt;
    let stopping = AtomicBool::new(false);
    for _ in 0..100 {
      runtime.tick(&stopping).await.unwrap();
    }
    assert!(runtime.manager.is_none());
    assert_eq!(runtime.failures, 1);
    assert_eq!(runtime.next_attempt, deadline);
  }

  #[test]
  fn transport_loss_reconnects_but_authorization_and_native_errors_do_not() {
    for error in [
      TransportError::NotConnected,
      TransportError::Timeout,
      TransportError::Connection("Response channel closed".into()),
      TransportError::Io(std::io::Error::from(std::io::ErrorKind::ConnectionReset)),
      TransportError::Tls("unexpected EOF".into()),
    ] {
      let error = anyhow::Error::new(error).context("WG responder refresh");
      assert!(control_connection_failed(&error), "{error:#}");
    }
    for error in [
      anyhow::Error::new(TransportError::Protocol(
        "WG path coordination access revoked".into(),
      )),
      anyhow::Error::new(TransportError::AuthFailed("session replaced".into())),
      anyhow::Error::new(TransportError::Certificate("certificate expired".into())),
      anyhow::anyhow!("Snapshot rejected: network not found"),
      anyhow::anyhow!("WG authority unavailable; legacy writer remains fenced"),
      anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionReset))
        .context("native WG apply failed"),
    ] {
      assert!(!control_connection_failed(&error), "{error:#}");
    }
  }

  #[test]
  fn every_linux_controller_starts_with_responder_permissions() {
    let controller = new_controller("/unused/linux-responder".into());
    assert_eq!(controller.path_role(), PathRole::Responder);
    assert!(!controller.is_shutting_down());
  }

  #[test]
  fn periodic_runtime_reads_only_reconcile_topology_on_event_or_fallback() {
    assert!(!ordinary_snapshot_reconcile_due(
      PendingEvent::None,
      Duration::from_secs(5)
    ));
    assert!(ordinary_snapshot_reconcile_due(
      PendingEvent::Refresh,
      Duration::from_secs(5)
    ));
    assert!(ordinary_snapshot_reconcile_due(
      PendingEvent::None,
      Duration::from_secs(30)
    ));
  }
}

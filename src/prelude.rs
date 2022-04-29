use futures::Stream;
use kube::runtime::{controller, reflector::ObjectRef, watcher};

pub use super::secret_types::AutoSecretType;
pub use color_eyre::Result;
pub use futures::StreamExt;
pub use k8s_openapi::{api::core::v1::Secret, ByteString};
pub use kube::{
  api::{ListParams, Patch, PatchParams},
  core::ObjectMeta,
  runtime::{
    controller::{Action, Context},
    Controller,
  },
  Api, Client, CustomResource, CustomResourceExt, Resource,
};
pub use schemars::JsonSchema;
pub use serde::{Deserialize, Serialize};
pub use std::{
  collections::{BTreeMap, HashMap},
  hash::{Hash, Hasher},
  io::BufRead,
  sync::Arc,
  time::Duration,
};
pub use thiserror::Error;
pub use tracing::{info, warn};
pub use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};
pub use tracing_tree::HierarchicalLayer;

pub fn setup_logging() -> Result<()> {
  let env_log = format!("{}=info", env!("CARGO_PKG_NAME").replace("-", "_"));
  println!("log: {env_log}");
  std::env::set_var("RUST_LOG", &env_log);
  color_eyre::install()?;
  Registry::default()
    .with(EnvFilter::from_default_env())
    .with(HierarchicalLayer::new(2).with_targets(true).with_bracketed_fields(true))
    .init();

  Ok(())
}

pub fn stdin_newlines() -> impl Stream<Item = ()> + Send + Sync {
  // reconcile everything anew when pressing enter
  let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);
  // Using a regular background thread since tokio::io::stdin() doesn't allow aborting reads,
  // and its worker prevents the Tokio runtime from shutting down.
  std::thread::spawn(move || {
    for _ in std::io::BufReader::new(std::io::stdin()).lines() {
      let _ = reload_tx.try_send(());
    }
  });

  reload_rx.map(|_| ())
}

pub async fn log_reconciler_result(
  res: Result<(ObjectRef<super::AutoSecret>, Action), controller::Error<ControllerError, watcher::Error>>,
) {
  match res {
    Ok((o, _)) => info!("reconciled {}/{}", o.namespace.as_deref().unwrap_or("NIL"), o.name),
    Err(e) => warn!("reconcile failed: {}", e),
  }
}

pub trait ControllerExt {
  fn handle_signals(self) -> Self;
}

impl ControllerExt for Controller<super::AutoSecret> {
  fn handle_signals(self) -> Self {
    self.reconcile_all_on(stdin_newlines()).shutdown_on_signal()
  }
}

#[async_trait::async_trait]
pub trait ClientExt {
  async fn get_secret_or_default(&self, auto_secret: &super::AutoSecret) -> Result<Secret, ControllerError>;
}

#[async_trait::async_trait]
impl ClientExt for Client {
  async fn get_secret_or_default(&self, auto_secret: &super::AutoSecret) -> Result<Secret, ControllerError> {
    let oref = auto_secret.controller_owner_ref(&()).unwrap();
    let name = auto_secret.name()?;
    let namespace = auto_secret.namespace()?;

    let secret_api = Api::<Secret>::namespaced(self.clone(), &namespace);
    let mut secret = Secret {
      metadata: ObjectMeta {
        name: Some(name.clone()),
        namespace: Some(namespace.clone()),
        owner_references: Some(vec![oref]),
        ..ObjectMeta::default()
      },
      ..Default::default()
    };

    let existing_secret = get_secret(&secret_api, &name).await?;
    if let Some(existing_secret) = existing_secret {
      secret.metadata.annotations = existing_secret.metadata.annotations.map(|mut annotations| {
        annotations.retain(|k, _| k.starts_with(ANNOTATION_PREFIX));
        annotations
      });

      secret.data = existing_secret.data;
    }

    Ok(secret)
  }
}

#[async_trait::async_trait]
pub trait AutoSecretExt {
  fn namespace(&self) -> Result<String, ControllerError>;
  fn name(&self) -> Result<String, ControllerError>;
  fn secrets(&self) -> HashMap<String, super::AutoSecretType>;
}

#[async_trait::async_trait]
impl AutoSecretExt for super::AutoSecret {
  fn namespace(&self) -> Result<String, ControllerError> {
    self
      .metadata
      .namespace
      .clone()
      .ok_or(ControllerError::MissingObjectKey(".metadata.namespace"))
  }

  fn name(&self) -> Result<String, ControllerError> {
    self
      .metadata
      .name
      .clone()
      .ok_or(ControllerError::MissingObjectKey(".metadata.name"))
  }

  fn secrets(&self) -> HashMap<String, super::AutoSecretType> {
    self.spec.secrets.clone()
  }
}

pub enum SecretStatus {
  Missing,
  Outdated,
  Matches,
}

#[async_trait::async_trait]
pub trait SecretExt {
  fn retain(&mut self, filter: impl FnMut(&str, &ByteString) -> bool) -> bool;
  fn secret_status(&self, name: &str, spec: &super::AutoSecretType) -> SecretStatus;
  fn set_secret(&mut self, name: &str, spec: &super::AutoSecretType);
  async fn apply(self, client: Client) -> Result<(), ControllerError>;
}

#[async_trait::async_trait]
impl SecretExt for Secret {
  fn retain(&mut self, mut filter: impl FnMut(&str, &ByteString) -> bool) -> bool {
    let annotations = self.metadata.annotations.get_or_insert_with(Default::default);
    let data = self.data.get_or_insert_with(Default::default);

    let to_remove = data
      .iter()
      .filter(|(n, v)| filter(*n, *v))
      .map(|(n, _)| n)
      .cloned()
      .collect::<Vec<_>>();

    let modified = !to_remove.is_empty();

    for name in to_remove {
      remove_secret(annotations, data, &*name);
    }

    modified
  }

  fn secret_status(&self, name: &str, spec: &super::AutoSecretType) -> SecretStatus {
    let annotations = match self.metadata.annotations.as_ref() {
      None => return SecretStatus::Missing,
      Some(v) => v,
    };

    let annotation_name = annotation_name(name);
    let expected_hash = annotations.get(&annotation_name).cloned();
    let actual_hash = hash(spec);

    match expected_hash {
      Some(expected) if expected == actual_hash => SecretStatus::Matches,
      Some(_) => SecretStatus::Outdated,
      None => SecretStatus::Missing,
    }

    // let value = ByteString(conf.generate().into_bytes());
    // annotations.insert(annotation_name, actual_hash);
    // data.insert(name.into(), value);
  }

  fn set_secret(&mut self, name: &str, spec: &super::AutoSecretType) {
    let annotations = self.metadata.annotations.get_or_insert_with(Default::default);
    let data = self.data.get_or_insert_with(Default::default);
    let value = ByteString(spec.generate().into_bytes());
    let annotation_name = annotation_name(name);
    let actual_hash = hash(spec);

    annotations.insert(annotation_name, actual_hash);
    data.insert(name.into(), value);
  }

  async fn apply(self, client: Client) -> Result<(), ControllerError> {
    let namespace = self.metadata.namespace.clone().expect("secret must have namespace");
    let name = self.metadata.name.clone().expect("secret must have name");
    let secret_api = Api::<Secret>::namespaced(client, &namespace);

    patch_secret(secret_api, &name, self).await
  }
}

const ANNOTATION_PREFIX: &str = "autosecrets.webstep.no/";

fn annotation_name(name: &str) -> String {
  format!("{ANNOTATION_PREFIX}{name}")
}

#[tracing::instrument(skip_all, fields(secret.name = name))]
async fn get_secret(secret_api: &Api<Secret>, name: &str) -> Result<Option<Secret>, ControllerError> {
  secret_api.get_opt(name).await.map_err(ControllerError::SecretGetFailed)
}

#[tracing::instrument(skip_all, fields(secret.name = name))]
async fn patch_secret(secret_api: Api<Secret>, name: &str, secret: Secret) -> Result<(), ControllerError> {
  secret_api
    .patch(
      name,
      &PatchParams::apply("autosecrets.webstep.no").force(),
      &Patch::Apply(&secret),
    )
    .await
    .map_err(ControllerError::SecretApplyFailed)?;

  Ok(())
}

fn remove_secret(annotations: &mut BTreeMap<String, String>, data: &mut BTreeMap<String, ByteString>, name: &str) {
  info!("removing secret {}", name);
  annotations.remove(&annotation_name(name));
  data.remove(name);
}

fn hash(value: &impl Hash) -> String {
  let mut hasher = seahash::SeaHasher::new();
  value.hash(&mut hasher);
  let value = hasher.finish();
  hex::encode(value.to_le_bytes())
}

#[derive(Debug, Error)]
pub enum ControllerError {
  #[error("Failed to get secret: {0}")]
  SecretGetFailed(#[source] kube::Error),

  #[error("Failed to apply secret: {0}")]
  SecretApplyFailed(#[source] kube::Error),

  #[error("MissingObjectKey: {0}")]
  MissingObjectKey(&'static str),
}

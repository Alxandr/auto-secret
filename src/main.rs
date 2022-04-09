mod secret_types;

use color_eyre::Result;
use futures::StreamExt;
use k8s_openapi::{api::core::v1::Secret, ByteString};
use kube::{
  api::{ListParams, Patch, PatchParams},
  core::ObjectMeta,
  runtime::{
    controller::{Action, Context},
    Controller,
  },
  Api, Client, CustomResource, CustomResourceExt, Resource,
};
use schemars::JsonSchema;
use secret_types::AutoSecretType;
use serde::{Deserialize, Serialize};
use std::{
  collections::{BTreeMap, HashMap},
  hash::{Hash, Hasher},
  io::BufRead,
  sync::Arc,
  time::Duration,
};
use thiserror::Error;
use tracing::{debug, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Registry};
use tracing_tree::HierarchicalLayer;

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "webstep.no", version = "v1alpha1", kind = "AutoSecret")]
#[kube(shortname = "as", namespaced)]
struct AutoSecretSpec {
  secrets: HashMap<String, AutoSecretType>,
}

#[tokio::main]
async fn main() -> Result<()> {
  color_eyre::install()?;
  // 👇 new!
  Registry::default()
    .with(EnvFilter::from_default_env())
    .with(HierarchicalLayer::new(2).with_targets(true).with_bracketed_fields(true))
    .init();

  let args = argwerk::args! {
    /// auto-secret controller
    "auto-secret [--crd|-h]" {
      help: bool,
      crd: bool,
    }

    /// Print the crd.
    ["--crd"] => {
      crd = true
    }

    /// Print this help.
    ["-h" | "--help"] => {
      println!("{}", HELP);
      help = true;
    }
  }?;

  if args.help {
    return Ok(());
  }

  if args.crd {
    let crd = AutoSecret::crd();
    let yaml = serde_yaml::to_string(&crd)?;
    println!("{yaml}");

    return Ok(());
  }

  let client = Client::try_default().await?;

  let autosecrets = Api::<AutoSecret>::all(client.clone());
  let secrets = Api::<Secret>::all(client.clone());

  info!("starting autosecret-controller");
  info!("press <enter> to force a reconciliation of all objects");

  let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);
  // Using a regular background thread since tokio::io::stdin() doesn't allow aborting reads,
  // and its worker prevents the Tokio runtime from shutting down.
  std::thread::spawn(move || {
    for _ in std::io::BufReader::new(std::io::stdin()).lines() {
      let _ = reload_tx.try_send(());
    }
  });

  Controller::new(autosecrets, ListParams::default())
    .owns(secrets, ListParams::default())
    .reconcile_all_on(reload_rx.map(|_| ()))
    .shutdown_on_signal()
    .run(reconcile, error_policy, Context::new(client))
    .for_each(|res| async move {
      match res {
        Ok((o, _)) => info!("reconciled {}/{}", o.namespace.as_deref().unwrap_or("NIL"), o.name),
        Err(e) => warn!("reconcile failed: {}", e),
      }
    })
    .await;

  info!("controller terminated");
  Ok(())
}

/// Controller triggers this whenever our main object or our children changed
#[tracing::instrument(skip_all, fields(
  resource.namespace = resource.metadata.namespace.as_deref(),
  resource.name = resource.metadata.name.as_deref(),
))]
async fn reconcile(resource: Arc<AutoSecret>, ctx: Context<Client>) -> Result<Action, ControllerError> {
  let client = ctx.get_ref().clone();
  let namespace = resource
    .metadata
    .namespace
    .as_deref()
    .ok_or(ControllerError::MissingObjectKey(".metadata.namespace"))?;

  let name = resource
    .metadata
    .name
    .as_deref()
    .ok_or(ControllerError::MissingObjectKey(".metadata.name"))?;

  let secret_api = Api::<Secret>::namespaced(client.clone(), namespace);

  let oref = resource.controller_owner_ref(&()).unwrap();
  let mut secret = Secret {
    metadata: ObjectMeta {
      name: Some(name.into()),
      owner_references: Some(vec![oref]),
      ..ObjectMeta::default()
    },
    ..Default::default()
  };

  let existing_secret = get_secret(&secret_api, name).await?;
  if let Some(existing_secret) = existing_secret {
    secret.metadata.annotations = existing_secret.metadata.annotations.map(|mut annotations| {
      annotations.retain(|k, _| k.starts_with(ANNOTATION_PREFIX));
      annotations
    });

    secret.data = existing_secret.data;
  }

  let annotations = secret.metadata.annotations.get_or_insert_with(Default::default);
  let data = secret.data.get_or_insert_with(Default::default);

  let to_remove: Vec<_> = data
    .keys()
    .filter(|name| !resource.spec.secrets.contains_key(&**name))
    .map(String::from)
    .collect();

  for name in to_remove {
    remove_secret(annotations, data, &*name);
  }

  for (name, conf) in &resource.spec.secrets {
    check_secret(annotations, data, &**name, conf);
  }

  patch_secret(secret_api, name, secret).await?;

  Ok(Action::await_change())
}

fn check_secret(
  annotations: &mut BTreeMap<String, String>,
  data: &mut BTreeMap<String, ByteString>,
  name: &str,
  conf: &AutoSecretType,
) {
  let annotation_name = annotation_name(name);
  let expected_hash = annotations.get(&annotation_name).cloned();
  let actual_hash = hash(conf);

  match expected_hash {
    Some(expected) if expected == actual_hash => {
      debug!("skipping secret {} due to same hash", name);
      return;
    }
    Some(_) => info!("updating secret {} due to hash change", name),
    None => info!("creating new secret {}", name),
  }

  let value = ByteString(conf.generate().into_bytes());
  annotations.insert(annotation_name, actual_hash);
  data.insert(name.into(), value);
}

fn remove_secret(annotations: &mut BTreeMap<String, String>, data: &mut BTreeMap<String, ByteString>, name: &str) {
  info!("removing secret {}", name);
  annotations.remove(&annotation_name(name));
  data.remove(name);
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

/// The controller triggers this on reconcile errors
#[tracing::instrument(skip_all)]
fn error_policy(_: &ControllerError, _: Context<Client>) -> Action {
  Action::requeue(Duration::from_secs(15))
}

fn hash(value: &impl Hash) -> String {
  let mut hasher = seahash::SeaHasher::new();
  value.hash(&mut hasher);
  let value = hasher.finish();
  hex::encode(value.to_le_bytes())
}

#[derive(Debug, Error)]
enum ControllerError {
  #[error("Failed to get secret: {0}")]
  SecretGetFailed(#[source] kube::Error),

  #[error("Failed to apply secret: {0}")]
  SecretApplyFailed(#[source] kube::Error),

  #[error("MissingObjectKey: {0}")]
  MissingObjectKey(&'static str),
}

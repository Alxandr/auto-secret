mod prelude;
mod secret_types;

use prelude::*;

#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(group = "webstep.no", version = "v1alpha1", kind = "AutoSecret")]
#[kube(shortname = "as", namespaced)]
pub struct AutoSecretSpec {
  secrets: HashMap<String, AutoSecretType>,
}

#[tokio::main]
async fn main() -> Result<()> {
  setup_logging()?;

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

  Controller::new(autosecrets, ListParams::default())
    .owns(secrets, ListParams::default())
    .handle_signals()
    .run(reconcile, error_policy, Context::new(client))
    .for_each(log_reconciler_result)
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

  // get existing secret (from k8s) or create new empty (in-memory) secret
  // with the correct metadata.
  let mut secret = client.get_secret_or_default(&resource).await?;

  // get secret value pairs from the spec
  let spec_secrets = resource.secrets();

  // remove (in-memory) all secrets from the k8s secret
  // that does not exist in the spec
  secret.retain(|name, _| !spec_secrets.contains_key(name));

  // update or create missing secrets in the k8s secret
  // that do exist in the spec
  for (name, secret_spec) in &spec_secrets {
    match secret.secret_status(name, secret_spec) {
      SecretStatus::Missing => info!("creating new secret {}", name),
      SecretStatus::Outdated => info!("updating secret {} due to hash change", name),
      SecretStatus::Matches => {
        info!("skipping secret {} due to same hash", name);
        continue;
      }
    }

    secret.set_secret(name, secret_spec);
  }

  // apply secret in k8s
  secret.apply(client).await?;

  Ok(Action::await_change())
}

// copy in everything below this line

/// The controller triggers this on reconcile errors
#[tracing::instrument(skip_all)]
fn error_policy(_: &ControllerError, _: Context<Client>) -> Action {
  Action::requeue(Duration::from_secs(15))
}

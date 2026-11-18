//! Commands that start, stop, roll back, and remove apps that the
//! configuration does not manage.
//!
//! The configuration determines the state of a managed app, and the next check
//! would undo a change of it, so the commands reject managed apps. Each command
//! changes the app under the device lock and then requests a check, which
//! publishes the new status. Like other changes of apps, a change runs to
//! completion once it has started, even if the command is cancelled, for
//! example because the connection is lost.

use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::AppControl;
use super::rugix::RugixApps;

/// Command that changes an app that the configuration does not manage.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AppCommand {
    Start,
    Stop,
    Rollback,
    Remove,
}

impl AppCommand {
    pub(crate) const ALL: [Self; 4] = [Self::Start, Self::Stop, Self::Rollback, Self::Remove];

    /// Name of the device command.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Start => "nexigon.rugix-apps.start",
            Self::Stop => "nexigon.rugix-apps.stop",
            Self::Rollback => "nexigon.rugix-apps.rollback",
            Self::Remove => "nexigon.rugix-apps.remove",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Start => "Start an app that the configuration does not manage.",
            Self::Stop => "Stop an app that the configuration does not manage.",
            Self::Rollback => {
                "Roll back an app that the configuration does not manage to its previous version."
            }
            Self::Remove => {
                "Remove an app that the configuration does not manage, including its data."
            }
        }
    }

    /// JSON Schema of the input, which names the app.
    pub(crate) fn input_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "app": { "type": "string", "description": "Name of the app." }
            },
            "required": ["app"]
        })
    }
}

/// Input of an app command.
#[derive(Deserialize)]
struct Input {
    app: String,
}

impl AppControl {
    /// Run an app command and return its error message if it fails.
    pub(crate) async fn run_command(
        &self,
        command: AppCommand,
        input: Value,
    ) -> Result<(), String> {
        let Input { app } =
            serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
        check_app_name(&app)?;
        // A check that runs while the command waits may change the managed apps.
        let guard = self.device_lock.clone().lock_owned().await;
        match self.managed_apps() {
            None => return Err("the agent has not checked the apps yet".to_owned()),
            Some(managed) if managed.contains(&app) => {
                return Err(format!(
                    "dev.nexigon.rugix-apps.config manages app {app}, so change its configuration instead"
                ));
            }
            Some(_) => {}
        }
        let check_requested = self.check_requested.clone();
        let change = tokio::spawn(async move {
            let rugix = RugixApps;
            let result = match command {
                AppCommand::Start => rugix.start(&app).await,
                AppCommand::Stop => rugix.stop(&app).await,
                AppCommand::Rollback => rugix.rollback(&app).await,
                AppCommand::Remove => rugix.remove(&app).await,
            };
            drop(guard);
            check_requested.notify_one();
            result
        });
        match change.await {
            Ok(result) => result.map_err(|error| format!("{error:#}")),
            Err(error) => Err(format!("the change of the app failed: {error}")),
        }
    }
}

/// Reject an app name that is empty or that Rugix Ctrl would read as an option.
pub(crate) fn check_app_name(app: &str) -> Result<(), String> {
    if app.is_empty() || app.starts_with('-') {
        return Err(format!("invalid app name {app:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Commands reject invalid input, apps before the first check, and apps
    /// that the configuration lists.
    #[tokio::test]
    async fn commands_reject_managed_apps() {
        let control = AppControl::default();
        let stop = |app: &str| control.run_command(AppCommand::Stop, json!({ "app": app }));

        let error = control
            .run_command(AppCommand::Stop, json!({}))
            .await
            .unwrap_err();
        assert!(error.starts_with("invalid input"), "{error}");
        let error = stop("controller").await.unwrap_err();
        assert!(error.contains("not checked"), "{error}");

        let error = stop("--help").await.unwrap_err();
        assert!(error.contains("invalid app name"), "{error}");

        control.set_managed_apps(["controller".to_owned()].into());
        let error = stop("controller").await.unwrap_err();
        assert!(error.contains("manages app controller"), "{error}");
    }

    /// A command that waits for the device lock sees the managed apps of a
    /// check that held the lock.
    #[tokio::test]
    async fn commands_check_managed_apps_after_waiting() {
        let control = AppControl::default();
        control.set_managed_apps(Default::default());
        let guard = control.device_lock.clone().lock_owned().await;
        let command = tokio::spawn({
            let control = control.clone();
            async move {
                control
                    .run_command(AppCommand::Remove, json!({ "app": "controller" }))
                    .await
            }
        });
        tokio::task::yield_now().await;
        control.set_managed_apps(["controller".to_owned()].into());
        drop(guard);
        let error = command.await.unwrap().unwrap_err();
        assert!(error.contains("manages app controller"), "{error}");
    }
}

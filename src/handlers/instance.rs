use axum::routing::{delete, get, post};
use axum::Router;
use axum::{extract::Path, Json};
use axum_auth::AuthBearer;

use color_eyre::eyre::{eyre, Context};
use tracing::error;

use crate::auth::user::UserAction;
use crate::error::{Error, ErrorKind};
use crate::events::{CausedBy, Event, ProgressionEndValue, ProgressionStartValue};

use minecraft::FlavourKind;

use crate::implementations::minecraft::MinecraftInstance;
use crate::prelude::PATH_TO_INSTANCES;
use crate::traits::t_configurable::manifest::ManifestValue;
use crate::traits::{t_configurable::TConfigurable, t_server::TServer, InstanceInfo, TInstance};

use crate::types::{DotLodestoneConfig, InstanceUuid};
use crate::{implementations::minecraft, traits::t_server::State, AppState};

use super::instance_setup_configs::HandlerGameType;

pub async fn get_instance_list(
    axum::extract::State(state): axum::extract::State<AppState>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<Vec<InstanceInfo>>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    let mut list_of_configs: Vec<InstanceInfo> = Vec::new();

    let instances = state.instances.lock().await;
    for instance in instances.values() {
        if requester.can_perform_action(&UserAction::ViewInstance(instance.uuid().await)) {
            list_of_configs.push(instance.get_instance_info().await);
        }
    }

    list_of_configs.sort_by(|a, b| a.creation_time.cmp(&b.creation_time));

    Ok(Json(list_of_configs))
}

pub async fn get_instance_info(
    Path(uuid): Path<InstanceUuid>,
    axum::extract::State(state): axum::extract::State<AppState>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<InstanceInfo>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;

    let instances = state.instances.lock().await;

    let instance = instances.get(&uuid).ok_or_else(|| Error {
        kind: ErrorKind::NotFound,
        source: eyre!("Instance not found"),
    })?;

    requester.try_action(&UserAction::ViewInstance(uuid.clone()))?;
    Ok(Json(instance.get_instance_info().await))
}

pub async fn create_instance(
    axum::extract::State(state): axum::extract::State<AppState>,
    AuthBearer(token): AuthBearer,
    Path(game_type): Path<HandlerGameType>,
    Json(manifest_value): Json<ManifestValue>,
) -> Result<Json<InstanceUuid>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::CreateInstance)?;
    let mut perm = requester.permissions;

    let mut instance_uuid = InstanceUuid::default();

    for uuid in state.instances.lock().await.keys() {
        if let Some(uuid) = uuid.as_ref().get(0..8) {
            if uuid == &instance_uuid.no_prefix()[0..8] {
                instance_uuid = InstanceUuid::default();
            }
        }
    }

    let instance_uuid = instance_uuid;

    let flavour = match game_type {
        HandlerGameType::MinecraftJavaVanilla => FlavourKind::Vanilla,
        HandlerGameType::MinecraftForge => FlavourKind::Forge,
        HandlerGameType::MinecraftFabric => FlavourKind::Fabric,
        HandlerGameType::MinecraftPaper => FlavourKind::Paper,
    };

    let setup_config = MinecraftInstance::construct_setup_config(manifest_value, flavour).await?;

    let setup_path = PATH_TO_INSTANCES.with(|path| {
        path.join(format!(
            "{}-{}",
            setup_config.name,
            &instance_uuid.no_prefix()[0..8]
        ))
    });

    tokio::fs::create_dir_all(&setup_path)
        .await
        .context("Failed to create instance directory")?;

    let dot_lodestone_config = DotLodestoneConfig::new(instance_uuid.clone(), game_type.into());

    // write dot lodestone config

    tokio::fs::write(
        setup_path.join(".lodestone_config"),
        serde_json::to_string_pretty(&dot_lodestone_config).unwrap(),
    )
    .await
    .context("Failed to write .lodestone_config file")?;

    tokio::task::spawn({
        let uuid = instance_uuid.clone();
        let instance_name = setup_config.name.clone();
        let event_broadcaster = state.event_broadcaster.clone();
        let port = setup_config.port;
        let flavour = setup_config.flavour.clone();
        let caused_by = CausedBy::User {
            user_id: requester.uid.clone(),
            user_name: requester.username.clone(),
        };
        async move {
            let (progression_start_event, event_id) = Event::new_progression_event_start(
                &format!("Setting up Minecraft server {instance_name}"),
                Some(uuid.clone()),
                Some(10.0),
                Some(ProgressionStartValue::InstanceCreation {
                    instance_uuid: uuid.clone(),
                    instance_name: instance_name.clone(),
                    port,
                    flavour: flavour.to_string(),
                    game_type: "minecraft".to_string(),
                }),
                caused_by,
            );
            event_broadcaster.send(progression_start_event);
            let minecraft_instance = match minecraft::MinecraftInstance::new(
                setup_config.clone(),
                dot_lodestone_config,
                setup_path.clone(),
                event_id,
                state.event_broadcaster.clone(),
                state.macro_executor.clone(),
            )
            .await
            {
                Ok(v) => {
                    event_broadcaster.send(Event::new_progression_event_end(
                        event_id,
                        true,
                        Some("Instance created successfully"),
                        Some(ProgressionEndValue::InstanceCreation(
                            v.get_instance_info().await,
                        )),
                    ));
                    v
                }
                Err(e) => {
                    event_broadcaster.send(Event::new_progression_event_end(
                        event_id,
                        false,
                        Some(&format!("Instance creation failed: {e}")),
                        None,
                    ));
                    crate::util::fs::remove_dir_all(setup_path)
                        .await
                        .context("Failed to remove directory after instance creation failed")
                        .unwrap();
                    return;
                }
            };
            let mut port_manager = state.port_manager.lock().await;
            port_manager.add_port(setup_config.port);
            perm.can_start_instance.insert(uuid.clone());
            perm.can_stop_instance.insert(uuid.clone());
            perm.can_view_instance.insert(uuid.clone());
            perm.can_read_instance_file.insert(uuid.clone());
            perm.can_write_instance_file.insert(uuid.clone());
            // ignore errors since we don't care if the permissions update fails
            let _ = state
                .users_manager
                .write()
                .await
                .update_permissions(&requester.uid, perm, CausedBy::System)
                .await
                .map_err(|e| {
                    error!("Failed to update permissions: {:?}", e);
                    e
                });
            state
                .instances
                .lock()
                .await
                .insert(uuid.clone(), minecraft_instance.into());
        }
    });
    Ok(Json(instance_uuid))
}

pub async fn delete_instance(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(uuid): Path<InstanceUuid>,
    AuthBearer(token): AuthBearer,
) -> Result<Json<()>, Error> {
    let requester = state.users_manager.read().await.try_auth_or_err(&token)?;
    requester.try_action(&UserAction::DeleteInstance)?;
    let mut instances = state.instances.lock().await;
    let caused_by = CausedBy::User {
        user_id: requester.uid.clone(),
        user_name: requester.username.clone(),
    };
    if let Some(instance) = instances.get(&uuid) {
        if !(instance.state().await == State::Stopped) {
            Err(Error {
                kind: ErrorKind::BadRequest,
                source: eyre!("Instance must be stopped before deletion"),
            })
        } else {
            let (progression_event_start, event_id) = Event::new_progression_event_start(
                &format!("Deleting instance {}", instance.name().await),
                Some(uuid.clone()),
                Some(10.0),
                None,
                caused_by,
            );
            let event_broadcaster = state.event_broadcaster.clone();
            event_broadcaster.send(progression_event_start);
            tokio::fs::remove_file(instance.path().await.join(".lodestone_config"))
                .await
                .map_err(|e| {
                    event_broadcaster.send(Event::new_progression_event_end(
                        event_id,
                        false,
                        Some("Failed to delete .lodestone_config. Instance not deleted"),
                        None,
                    ));
                    Err::<(), std::io::Error>(e)
                        .context("Failed to delete .lodestone_config file. Instance not deleted")
                        .unwrap_err()
                })?;
            state
                .port_manager
                .lock()
                .await
                .deallocate(instance.port().await);
            let instance_path = instance.path().await;
            instances.remove(&uuid);
            drop(instances);
            let res = crate::util::fs::remove_dir_all(instance_path).await;
            match &res {
                Ok(_) => event_broadcaster.send(Event::new_progression_event_end(
                    event_id,
                    true,
                    Some("Instance deleted successfully"),
                    Some(ProgressionEndValue::InstanceDelete {
                        instance_uuid: uuid.clone(),
                    }),
                )),
                Err(e) => {
                    event_broadcaster.send(Event::new_progression_event_end(
                        event_id,
                        false,
                        Some(&format!(
                            "Failed to delete some of all of instance's files : {e}"
                        )),
                        None,
                    ));
                }
            }
            res.map(|_| Json(()))
        }
    } else {
        Err(Error {
            kind: ErrorKind::NotFound,
            source: eyre!("Instance not found"),
        })
    }
}

pub fn get_instance_routes(state: AppState) -> Router {
    Router::new()
        .route("/instance/list", get(get_instance_list))
        .route("/instance/create/:game_type", post(create_instance))
        .route("/instance/:uuid", delete(delete_instance))
        .route("/instance/:uuid/info", get(get_instance_info))
        .with_state(state)
}

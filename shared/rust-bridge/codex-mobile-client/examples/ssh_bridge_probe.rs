use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use codex_app_server_protocol as upstream;
use codex_mobile_client::MobileClient;
use codex_mobile_client::ssh::{SshAuth, SshClient, SshCredentials};
use codex_mobile_client::ssh_bridge::SshBridgeTransport;
use codex_mobile_client::store::updates::AppStoreUpdateRecord;

#[derive(Debug)]
struct Args {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    key_path: Option<PathBuf>,
    passphrase: Option<String>,
    command: String,
    connect: bool,
    rpc_probe: bool,
    force_reconnect_probe: bool,
    live_turn_reconnect_probe: bool,
    runtime_kinds: Vec<String>,
    state_root: Option<String>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut host = None;
        let mut port = 22;
        let mut user = None;
        let mut password = None;
        let mut key_path = None;
        let mut passphrase = None;
        let mut command = "command -v pi >/dev/null 2>&1 && pi --version || true".to_string();
        let mut connect = false;
        let mut rpc_probe = false;
        let mut force_reconnect_probe = false;
        let mut live_turn_reconnect_probe = false;
        let mut runtime_kinds = vec!["pi".to_string()];
        let mut state_root = None;

        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--host" => host = args.next(),
                "--port" => {
                    let value = args.next().ok_or("missing --port value")?;
                    port = value
                        .parse()
                        .map_err(|error| format!("bad --port: {error}"))?;
                }
                "--user" => user = args.next(),
                "--password" => password = args.next(),
                "--key-path" => key_path = args.next().map(PathBuf::from),
                "--passphrase" => passphrase = args.next(),
                "--command" => command = args.next().ok_or("missing --command value")?,
                "--connect" => connect = true,
                "--rpc-probe" => {
                    connect = true;
                    rpc_probe = true;
                }
                "--force-reconnect-probe" => {
                    connect = true;
                    rpc_probe = true;
                    force_reconnect_probe = true;
                }
                "--live-turn-reconnect-probe" => {
                    connect = true;
                    live_turn_reconnect_probe = true;
                }
                "--runtime" => {
                    let value = args.next().ok_or("missing --runtime value")?;
                    runtime_kinds = value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(ToOwned::to_owned)
                        .collect();
                }
                "--state-root" => state_root = args.next(),
                "--help" | "-h" => return Err(Self::usage()),
                other => return Err(format!("unknown arg {other}\n{}", Self::usage())),
            }
        }

        if password.is_some() == key_path.is_some() {
            return Err(format!(
                "provide exactly one of --password or --key-path\n{}",
                Self::usage()
            ));
        }

        Ok(Self {
            host: host.ok_or_else(Self::usage)?,
            port,
            user: user.ok_or_else(Self::usage)?,
            password,
            key_path,
            passphrase,
            command,
            connect,
            rpc_probe,
            force_reconnect_probe,
            live_turn_reconnect_probe,
            runtime_kinds,
            state_root,
        })
    }

    fn usage() -> String {
        "usage: cargo run -p codex-mobile-client --example ssh_bridge_probe -- \\\n  --host HOST --port 22 --user USER (--password PW | --key-path PATH) \\\n  [--command CMD] [--connect] [--rpc-probe] [--force-reconnect-probe] \
  [--live-turn-reconnect-probe] [--runtime pi[,opencode]] [--state-root PATH]"
            .to_string()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse().map_err(|error| {
        eprintln!("{error}");
        std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
    })?;

    let auth = match (&args.password, &args.key_path) {
        (Some(password), None) => SshAuth::Password(password.clone()),
        (None, Some(path)) => SshAuth::PrivateKey {
            key_pem: std::fs::read_to_string(path)?,
            passphrase: args.passphrase.clone(),
        },
        _ => unreachable!("validated by Args::parse"),
    };

    let credentials = SshCredentials {
        host: args.host.clone(),
        port: args.port,
        username: args.user.clone(),
        auth,
        unlock_macos_keychain: false,
    };

    let client = Arc::new(
        SshClient::connect(
            credentials,
            Box::new(|_fingerprint| Box::pin(async { true })),
        )
        .await?,
    );
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-connect",
            "host": args.host,
            "port": args.port,
            "connected": client.is_connected(),
        })
    );

    let exec = client.exec(&args.command).await?;
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-exec",
            "exitCode": exec.exit_code,
            "stdout": exec.stdout,
            "stderr": exec.stderr,
        })
    );

    let availability = codex_mobile_client::ssh_bridge::probe_remote_agents(&client).await?;
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-probe-remote-agents",
            "availability": availability,
        })
    );

    if args.connect {
        let mobile = MobileClient::new();
        let state_root = args.state_root.unwrap_or_else(|| {
            std::env::temp_dir()
                .join(format!("litter-ssh-bridge-probe-{}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        });
        let outcome = mobile
            .connect_remote_over_ssh_bridges(
                Arc::clone(&client),
                "ssh-bridge-probe".to_string(),
                "SSH bridge probe".to_string(),
                args.host.clone(),
                state_root,
                args.runtime_kinds.clone(),
                SshBridgeTransport::Ephemeral,
            )
            .await?;
        let snapshot = mobile.app_snapshot();
        let server = snapshot.servers.get(&outcome.server_id);
        println!(
            "{}",
            serde_json::json!({
                "event": "ssh-connect-bridge-session",
                "serverId": outcome.server_id,
                "agentName": outcome.agent_name,
                "health": server.map(|server| format!("{:?}", server.health)),
                "runtimeKinds": server
                    .map(|server| server.agent_runtimes.iter().map(|runtime| runtime.kind.clone()).collect::<Vec<_>>())
                    .unwrap_or_default(),
            })
        );

        if args.live_turn_reconnect_probe {
            run_live_turn_reconnect_probe(&mobile, &outcome.server_id).await?;
        }

        if args.rpc_probe {
            let model_response: upstream::ModelListResponse = mobile
                .request_typed_for_server_runtime(
                    &outcome.server_id,
                    "pi".to_string(),
                    upstream::ClientRequest::ModelList {
                        request_id: upstream::RequestId::Integer(1),
                        params: upstream::ModelListParams {
                            cursor: None,
                            limit: Some(5),
                            include_hidden: None,
                        },
                    },
                )
                .await?;
            println!(
                "{}",
                serde_json::json!({
                    "event": "ssh-bridge-model-list",
                    "count": model_response.data.len(),
                    "nextCursor": model_response.next_cursor.is_some(),
                })
            );

            let thread_response: upstream::ThreadListResponse = mobile
                .request_typed_for_server_runtime(
                    &outcome.server_id,
                    "pi".to_string(),
                    upstream::ClientRequest::ThreadList {
                        request_id: upstream::RequestId::Integer(2),
                        params: upstream::ThreadListParams {
                            cursor: None,
                            limit: Some(5),
                            sort_key: None,
                            sort_direction: None,
                            model_providers: None,
                            source_kinds: None,
                            archived: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                        },
                    },
                )
                .await?;
            println!(
                "{}",
                serde_json::json!({
                    "event": "ssh-bridge-thread-list",
                    "count": thread_response.data.len(),
                    "nextCursor": thread_response.next_cursor.is_some(),
                })
            );

            if args.force_reconnect_probe {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "ssh-bridge-force-close-current",
                        "serverId": outcome.server_id,
                    })
                );
                mobile.abandon_alleycat_connections().await;
                let model_response = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    mobile.request_typed_for_server_runtime::<upstream::ModelListResponse>(
                        &outcome.server_id,
                        "pi".to_string(),
                        upstream::ClientRequest::ModelList {
                            request_id: upstream::RequestId::Integer(3),
                            params: upstream::ModelListParams {
                                cursor: None,
                                limit: Some(5),
                                include_hidden: None,
                            },
                        },
                    ),
                )
                .await??;
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "ssh-bridge-model-list-after-reconnect",
                        "count": model_response.data.len(),
                        "nextCursor": model_response.next_cursor.is_some(),
                    })
                );
            }
        }
    }

    client.disconnect().await;
    Ok(())
}

async fn run_live_turn_reconnect_probe(
    mobile: &MobileClient,
    server_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut updates = mobile.subscribe_app_updates();
    let thread_response: upstream::ThreadStartResponse = mobile
        .request_typed_for_server_runtime(
            server_id,
            "pi".to_string(),
            upstream::ClientRequest::ThreadStart {
                request_id: upstream::RequestId::Integer(100),
                params: upstream::ThreadStartParams {
                    service_name: Some("Litter SSH reconnect probe".to_string()),
                    ephemeral: Some(true),
                    ..Default::default()
                },
            },
        )
        .await?;
    let thread_id = thread_response.thread.id.clone();
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-thread-started",
            "threadId": thread_id,
            "model": thread_response.model,
            "modelProvider": thread_response.model_provider,
        })
    );

    let turn_response: upstream::TurnStartResponse = mobile
        .request_typed_for_server_runtime(
            server_id,
            "pi".to_string(),
            upstream::ClientRequest::TurnStart {
                request_id: upstream::RequestId::Integer(101),
                params: upstream::TurnStartParams {
                    thread_id: thread_id.clone(),
                    input: vec![upstream::UserInput::Text {
                        text: "For a transport reconnect probe, count from 1 to 80. Put each number on its own short line and do not use tools.".to_string(),
                        text_elements: Vec::new(),
                    }],
                    ..Default::default()
                },
            },
        )
        .await?;
    let turn_id = turn_response.turn.id.clone();
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-turn-started",
            "threadId": thread_id,
            "turnId": turn_id,
        })
    );

    let mut item_changes_before_close = 0usize;
    let mut deltas_before_close = 0usize;
    let mut completed_before_close = false;
    let wait_until = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        let now = tokio::time::Instant::now();
        if now >= wait_until || deltas_before_close > 0 || item_changes_before_close >= 2 {
            break;
        }
        match tokio::time::timeout(wait_until - now, updates.recv()).await {
            Ok(Ok(update)) => match update {
                AppStoreUpdateRecord::ThreadStreamingDelta { key, text, .. }
                    if key.server_id == server_id && key.thread_id == thread_id =>
                {
                    deltas_before_close += 1;
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "ssh-bridge-live-delta-before-close",
                            "threadId": thread_id,
                            "bytes": text.len(),
                        })
                    );
                }
                AppStoreUpdateRecord::ThreadItemChanged { key, .. }
                    if key.server_id == server_id && key.thread_id == thread_id =>
                {
                    item_changes_before_close += 1;
                }
                AppStoreUpdateRecord::ThreadUpserted { thread, .. }
                    if thread.key.server_id == server_id
                        && thread.key.thread_id == thread_id
                        && thread.active_turn_id.is_none()
                        && !thread.hydrated_conversation_items.is_empty() =>
                {
                    completed_before_close = true;
                    break;
                }
                _ => {}
            },
            Ok(Err(_)) | Err(_) => break,
        }
    }

    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-force-close",
            "threadId": thread_id,
            "turnId": turn_id,
            "deltasBeforeClose": deltas_before_close,
            "itemChangesBeforeClose": item_changes_before_close,
            "completedBeforeClose": completed_before_close,
        })
    );
    mobile.abandon_alleycat_connections().await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let manual_resubscribe = mobile
        .force_refresh_thread_authoritative(server_id, &thread_id)
        .await
        .map(|_| "ok".to_string())
        .unwrap_or_else(|error| error.to_string());
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-manual-resubscribe",
            "threadId": thread_id,
            "result": manual_resubscribe,
        })
    );

    let mut deltas_after_close = 0usize;
    let mut item_changes_after_close = 0usize;
    let mut completed_after_close = false;
    let wait_until = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    while tokio::time::Instant::now() < wait_until && !completed_after_close {
        let remaining = wait_until.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, updates.recv()).await {
            Ok(Ok(update)) => match update {
                AppStoreUpdateRecord::ThreadStreamingDelta { key, .. }
                    if key.server_id == server_id && key.thread_id == thread_id =>
                {
                    deltas_after_close += 1;
                }
                AppStoreUpdateRecord::ThreadItemChanged { key, .. }
                    if key.server_id == server_id && key.thread_id == thread_id =>
                {
                    item_changes_after_close += 1;
                }
                AppStoreUpdateRecord::ThreadUpserted { thread, .. }
                    if thread.key.server_id == server_id
                        && thread.key.thread_id == thread_id
                        && thread.active_turn_id.is_none()
                        && !thread.hydrated_conversation_items.is_empty() =>
                {
                    completed_after_close = true;
                }
                _ => {}
            },
            Ok(Err(_)) | Err(_) => break,
        }
    }

    let snapshot = mobile.app_snapshot();
    let key = codex_mobile_client::types::ThreadKey {
        server_id: server_id.to_string(),
        thread_id: thread_id.clone(),
    };
    let thread = snapshot.threads.get(&key);
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-turn-reconnect-result",
            "threadId": thread_id,
            "turnId": turn_id,
            "deltasBeforeClose": deltas_before_close,
            "deltasAfterClose": deltas_after_close,
            "itemChangesBeforeClose": item_changes_before_close,
            "itemChangesAfterClose": item_changes_after_close,
            "completedAfterClose": completed_after_close,
            "snapshotActiveTurnId": thread.and_then(|thread| thread.active_turn_id.clone()),
            "snapshotItemCount": thread.map(|thread| thread.items.len()).unwrap_or_default(),
        })
    );

    let turns_response: upstream::ThreadTurnsListResponse = mobile
        .request_typed_for_server_runtime(
            server_id,
            "pi".to_string(),
            upstream::ClientRequest::ThreadTurnsList {
                request_id: upstream::RequestId::Integer(102),
                params: upstream::ThreadTurnsListParams {
                    thread_id: thread_id.clone(),
                    cursor: None,
                    limit: Some(5),
                    sort_direction: Some(upstream::SortDirection::Desc),
                    items_view: Some(upstream::TurnItemsView::Full),
                },
            },
        )
        .await?;
    let server_turn = turns_response
        .data
        .iter()
        .find(|turn| turn.id == turn_id)
        .or_else(|| turns_response.data.first());
    let server_turn_status = server_turn.map(|turn| format!("{:?}", turn.status));
    let server_turn_item_count = server_turn.map(|turn| turn.items.len()).unwrap_or_default();
    let hydrated_server_items = codex_mobile_client::conversation::hydrate_turns(
        &turns_response.data,
        &codex_mobile_client::conversation::HydrationOptions::default(),
    );
    let server_item_ids: Vec<String> = hydrated_server_items
        .iter()
        .map(|item| item.id.clone())
        .collect();
    let mut sorted_server_item_ids = server_item_ids.clone();
    sorted_server_item_ids.sort();
    let mut unique_server_item_ids = sorted_server_item_ids.clone();
    unique_server_item_ids.dedup();
    let _ = mobile
        .load_thread_turns_page(server_id, &thread_id, None, Some(5))
        .await?;
    let repaired_snapshot = mobile.app_snapshot();
    let store_items = repaired_snapshot
        .threads
        .get(&key)
        .map(|thread| thread.items.clone())
        .unwrap_or_default();
    let store_item_ids: Vec<String> = store_items.iter().map(|item| item.id.clone()).collect();
    let mut sorted_store_item_ids = store_item_ids.clone();
    sorted_store_item_ids.sort();
    let mut unique_store_item_ids = sorted_store_item_ids.clone();
    unique_store_item_ids.dedup();
    let item_ids_match = sorted_store_item_ids == sorted_server_item_ids;
    let mut server_non_user_item_ids = hydrated_server_items
        .iter()
        .filter(|item| user_item_key(item).is_none())
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    server_non_user_item_ids.sort();
    let mut store_non_user_item_ids = store_items
        .iter()
        .filter(|item| user_item_key(item).is_none())
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    store_non_user_item_ids.sort();
    let mut server_user_keys = hydrated_server_items
        .iter()
        .filter_map(user_item_key)
        .collect::<Vec<_>>();
    server_user_keys.sort();
    server_user_keys.dedup();
    let mut store_user_keys = store_items
        .iter()
        .filter_map(user_item_key)
        .collect::<Vec<_>>();
    store_user_keys.sort();
    store_user_keys.dedup();
    let logical_content_match = server_non_user_item_ids
        .iter()
        .all(|id| store_non_user_item_ids.contains(id))
        && server_user_keys == store_user_keys;
    let store_item_details = store_items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "sourceTurnId": item.source_turn_id,
                "kind": item_kind(item),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({
            "event": "ssh-bridge-live-turn-content-check",
            "threadId": thread_id,
            "turnId": turn_id,
            "serverTurnCount": turns_response.data.len(),
            "serverTurnStatus": server_turn_status,
            "serverTurnItemCount": server_turn_item_count,
            "serverHydratedItemCount": server_item_ids.len(),
            "serverDuplicateItemIds": server_item_ids.len().saturating_sub(unique_server_item_ids.len()),
            "storeDuplicateItemIds": store_item_ids.len().saturating_sub(unique_store_item_ids.len()),
            "storeItemCount": store_item_ids.len(),
            "itemIdsMatch": item_ids_match,
            "logicalContentMatch": logical_content_match,
            "serverItemIds": server_item_ids,
            "storeItemIds": store_item_ids,
            "serverNonUserItemIds": server_non_user_item_ids,
            "storeNonUserItemIds": store_non_user_item_ids,
            "storeItemDetails": store_item_details,
        })
    );

    Ok(())
}

fn item_kind(
    item: &codex_mobile_client::conversation_uniffi::HydratedConversationItem,
) -> &'static str {
    use codex_mobile_client::conversation_uniffi::HydratedConversationItemContent;
    match &item.content {
        HydratedConversationItemContent::User(_) => "user",
        HydratedConversationItemContent::Assistant(_) => "assistant",
        HydratedConversationItemContent::Reasoning(_) => "reasoning",
        HydratedConversationItemContent::ProposedPlan(_) => "proposed_plan",
        HydratedConversationItemContent::CommandExecution(_) => "command_execution",
        HydratedConversationItemContent::CodeReview(_) => "code_review",
        HydratedConversationItemContent::TodoList(_) => "todo_list",
        HydratedConversationItemContent::FileChange(_) => "file_change",
        HydratedConversationItemContent::TurnDiff(_) => "turn_diff",
        HydratedConversationItemContent::McpToolCall(_) => "mcp_tool_call",
        HydratedConversationItemContent::DynamicToolCall(_) => "dynamic_tool_call",
        HydratedConversationItemContent::MultiAgentAction(_) => "multi_agent_action",
        HydratedConversationItemContent::WebSearch(_) => "web_search",
        HydratedConversationItemContent::ImageView(_) => "image_view",
        HydratedConversationItemContent::Widget(_) => "widget",
        HydratedConversationItemContent::UserInputResponse(_) => "user_input_response",
        HydratedConversationItemContent::Divider(_) => "divider",
        HydratedConversationItemContent::Error(_) => "error",
        HydratedConversationItemContent::Note(_) => "note",
        HydratedConversationItemContent::ImageGeneration(_) => "image_generation",
    }
}

fn user_item_key(
    item: &codex_mobile_client::conversation_uniffi::HydratedConversationItem,
) -> Option<(String, Vec<String>)> {
    match &item.content {
        codex_mobile_client::conversation_uniffi::HydratedConversationItemContent::User(data) => {
            Some((data.text.clone(), data.image_data_uris.clone()))
        }
        _ => None,
    }
}

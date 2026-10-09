//! obsidian-crud-mcp: give any AI agent access to an Obsidian vault over MCP.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use obsidian_crud_mcp::auth::OAuthProvider;
use obsidian_crud_mcp::config::Config;
use obsidian_crud_mcp::logging::{self, describe_error};
use obsidian_crud_mcp::mcp::{McpServer, ServerInfo};
use obsidian_crud_mcp::search::{AiSearchClient, AiSearchOptions, IndexState, SearchIndex};
use obsidian_crud_mcp::server::{Authenticator, router, sync_search_index, watch_vault};
use obsidian_crud_mcp::tools::{ToolContext, build_tools};
use obsidian_crud_mcp::vault::{AwsStore, LocalVault, ReadOnlyVault, S3Options, S3Vault, S3VaultOptions, VaultBackend};
use tracing::{debug, error, info, warn};

const SAVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const BASE_INSTRUCTIONS: &str = "Access and manage an Obsidian vault. You can read, write, list, search, move, and delete markdown notes. Every tool response includes an Obsidian deep link. Always show this link to the user using the format [obsidian://open?vault=...&file=...](obsidian://open?vault=...&file=...) so it is both clickable and visible as a URL.";

#[tokio::main]
async fn main() -> ExitCode {
    logging::init(std::env::var("LOG_LEVEL").ok().as_deref());
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            error!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// The vault: a local folder, or a local mirror of a Remotely Save S3 bucket.
async fn open_vault(config: &Config) -> Result<Arc<dyn VaultBackend>, String> {
    let vault_path = &config.vault_path;
    let fail = |e: std::io::Error| format!("Failed to open the vault at {}: {e}", vault_path.display());
    match &config.s3 {
        Some(s3) => {
            std::fs::create_dir_all(vault_path).map_err(fail)?;
            info!("S3 mode: bucket {}, mirror {}", s3.bucket, vault_path.display());
            let store = AwsStore::new(&S3Options {
                endpoint: s3.endpoint.clone(),
                region: s3.region.clone(),
                bucket: s3.bucket.clone(),
                prefix: s3.prefix.clone(),
                access_key_id: s3.access_key_id.clone(),
                secret_access_key: s3.secret_access_key.clone(),
            })
            .await;
            let options = S3VaultOptions {
                vault_path: vault_path.clone(),
                manifest_path: config.data_dir.join("s3-manifest.json"),
                poll_seconds: s3.poll_seconds,
                prefix: s3.prefix.clone(),
                write_folders: config.write_folders.clone(),
            };
            Ok(Arc::new(S3Vault::new(options, Arc::new(store)).map_err(fail)?))
        }
        None => {
            info!("Local mode: {}", vault_path.display());
            Ok(Arc::new(LocalVault::new(vault_path, config.write_folders.clone()).map_err(fail)?))
        }
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env().map_err(|e| e.to_string())?;
    if config.ignored_inline_instructions {
        warn!("MCP_INSTRUCTIONS_FILE is set; ignoring MCP_INSTRUCTIONS env var.");
    }

    // Load the index snapshot before the vault opens so changes feed it from the start.
    let index = Arc::new(SearchIndex::new(
        Some(config.data_dir.join("search-index.json")),
        config.index_passphrase.clone(),
        config.search_content_cache_chars,
    ));
    index.load_from_disk().await;
    debug!("Persisted metadata: {} notes", index.size());

    let mut vault = open_vault(&config).await?;
    // S3 mode: each poll reports what it downloaded or removed, content included,
    // so the index is updated without re-reading or watching the mirror folder.
    // Local mode: watch the folder for edits Obsidian makes.
    let subscription = vault.subscribe(index.clone());
    let watcher = subscription.is_none().then(|| watch_vault(vault.clone(), index.clone(), &config.vault_path));
    vault.init().await.map_err(|e| format!("Failed to open the vault: {}", describe_error(&e)))?;
    info!("Vault ready.");

    // READ_ONLY also hides the write tools; wrapping the backend makes any
    // write that bypasses the tools fail too.
    if config.read_only {
        vault = Arc::new(ReadOnlyVault::new(vault));
    }

    // Reconcile the index with the folder in the background (prunes deleted
    // notes, reads any whose mtime changed while the server was down).
    {
        let (vault, index) = (vault.clone(), index.clone());
        tokio::spawn(async move {
            if let Err(e) = sync_search_index(vault, index.clone()).await {
                index.set_state(IndexState::Failed);
                error!("Index rebuild failed: {}", describe_error(&e));
            }
        });
    }

    let oauth = match &config.auth_token {
        Some(token) => {
            let oauth = OAuthProvider::new(
                &config.base_url,
                token,
                Some(config.data_dir.join("auth-tokens.json")),
                config.refresh_days,
            );
            oauth.load_tokens().await;
            Some(oauth)
        }
        None => None,
    };
    let authenticator = match (&config.auth_token, &oauth) {
        (Some(token), Some(oauth)) => Authenticator::token(token, &config.base_url, oauth.clone()),
        _ => Authenticator::local_only(config.allowed_hosts.as_deref(), &config.host),
    };

    // Semantic search (optional): Cloudflare AI Search over the same bucket.
    let semantic = config.semantic.as_ref().map(|cf| {
        Arc::new(AiSearchClient::new(AiSearchOptions {
            account_id: cf.account_id.clone(),
            token: cf.token.clone(),
            namespace: cf.namespace.clone(),
            instance: cf.instance.clone(),
            prefix: config.s3.as_ref().map(|s| s.prefix.clone()).unwrap_or_default(),
            api_base: None,
        }))
    });
    match &config.semantic {
        Some(cf) => info!("Semantic search: Cloudflare AI Search instance '{}'.", cf.instance),
        None => info!("Semantic search off (set CF_ACCOUNT_ID, CF_AI_SEARCH_TOKEN, CF_AI_SEARCH_INSTANCE to enable)."),
    }

    let tools = build_tools(ToolContext {
        vault: vault.clone(),
        index: index.clone(),
        vault_name: config.vault_name.clone(),
        read_only: config.read_only,
        write_folders: config.write_folders.clone(),
        semantic: semantic.clone(),
    });
    let instructions = match &config.extra_instructions {
        Some(extra) => format!("{BASE_INSTRUCTIONS}\n\n{extra}"),
        None => BASE_INSTRUCTIONS.to_owned(),
    };
    let version = env!("CARGO_PKG_VERSION");
    let mcp = McpServer::new(
        ServerInfo { name: "obsidian-crud-mcp".to_owned(), version: version.to_owned(), instructions },
        tools,
    );
    let app = router(mcp, authenticator, oauth.as_ref());

    // Persist the index and OAuth state periodically and at shutdown.
    let persist = {
        let (index, oauth) = (index.clone(), oauth.clone());
        move || {
            let (index, oauth) = (index.clone(), oauth.clone());
            async move {
                index.save_to_disk().await;
                if let Some(oauth) = oauth {
                    oauth.cleanup();
                    oauth.save_tokens().await;
                }
            }
        }
    };
    let periodic = {
        let persist = persist.clone();
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + SAVE_INTERVAL, SAVE_INTERVAL);
            loop {
                ticks.tick().await;
                persist().await;
            }
        })
    };

    let listener = tokio::net::TcpListener::bind((config.host.as_str(), config.port))
        .await
        .map_err(|e| format!("Failed to listen on {}:{}: {e}", config.host, config.port))?;
    info!("obsidian-crud-mcp v{version} listening on port {}", config.port);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("Server error: {e}"))?;

    info!("Shutting down...");
    periodic.abort();
    if let Some(semantic) = &semantic {
        semantic.close();
    }
    drop(watcher);
    drop(subscription);
    persist().await;
    vault.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

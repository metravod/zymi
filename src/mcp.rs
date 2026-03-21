use std::path::Path;

use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
use rmcp::service::{DynService, Peer, RoleClient, RunningService, ServiceExt};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde::Deserialize;
use tokio::process::Command;

use crate::tools::mcp::McpTool;
use crate::tools::Tool;

#[derive(Deserialize)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: std::collections::HashMap<String, ServerConfig>,
}

#[derive(Deserialize)]
struct ServerConfig {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<std::collections::HashMap<String, String>>,
    url: Option<String>,
}

type DynRunningService = RunningService<RoleClient, Box<dyn DynService<RoleClient>>>;

pub struct McpManager {
    _handles: Vec<DynRunningService>,
}

impl McpManager {
    pub async fn init(memory_dir: &Path) -> (Self, Vec<Box<dyn Tool>>) {
        let config_path = memory_dir.join("mcp.json");

        let config = match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_json::from_str::<McpConfig>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!("Failed to parse mcp.json: {e}");
                    return (Self { _handles: vec![] }, vec![]);
                }
            },
            Err(_) => {
                log::info!("No mcp.json found, MCP disabled");
                return (Self { _handles: vec![] }, vec![]);
            }
        };

        let mut handles = Vec::new();
        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for (name, server_cfg) in &config.mcp_servers {
            match connect_server(name, server_cfg).await {
                Ok((service, peer)) => {
                    match peer.list_all_tools().await {
                        Ok(server_tools) => {
                            log::info!(
                                "MCP server '{}': discovered {} tools",
                                name,
                                server_tools.len()
                            );
                            for t in &server_tools {
                                log::info!("  - {}_{}", name, t.name);
                                tools.push(Box::new(McpTool::new(name, t, peer.clone())));
                            }
                        }
                        Err(e) => {
                            log::warn!("MCP server '{}': failed to list tools: {e}", name);
                        }
                    }
                    handles.push(service);
                }
                Err(e) => {
                    log::warn!("MCP server '{}': failed to connect: {e}", name);
                }
            }
        }

        (Self { _handles: handles }, tools)
    }

    /// Connect specific MCP servers by name from mcp.json.
    /// Stores handles internally and returns discovered tools.
    pub async fn connect_servers_by_name(
        &mut self,
        memory_dir: &Path,
        server_names: &[String],
    ) -> Vec<Box<dyn Tool>> {
        let config_path = memory_dir.join("mcp.json");

        let config = match std::fs::read_to_string(&config_path) {
            Ok(content) => match serde_json::from_str::<McpConfig>(&content) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!("Failed to parse mcp.json for hot-reload: {e}");
                    return vec![];
                }
            },
            Err(e) => {
                log::warn!("Cannot read mcp.json for hot-reload: {e}");
                return vec![];
            }
        };

        let mut tools: Vec<Box<dyn Tool>> = Vec::new();

        for name in server_names {
            let Some(server_cfg) = config.mcp_servers.get(name) else {
                log::warn!("MCP server '{name}' not found in mcp.json");
                continue;
            };

            match connect_server(name, server_cfg).await {
                Ok((service, peer)) => {
                    match peer.list_all_tools().await {
                        Ok(server_tools) => {
                            log::info!(
                                "MCP hot-reload '{}': discovered {} tools",
                                name,
                                server_tools.len()
                            );
                            for t in &server_tools {
                                log::info!("  - {}_{}", name, t.name);
                                tools.push(Box::new(McpTool::new(name, t, peer.clone())));
                            }
                        }
                        Err(e) => {
                            log::warn!("MCP hot-reload '{}': failed to list tools: {e}", name);
                        }
                    }
                    self._handles.push(service);
                }
                Err(e) => {
                    log::warn!("MCP hot-reload '{}': failed to connect: {e}", name);
                }
            }
        }

        tools
    }

    pub async fn shutdown(self) {
        for handle in self._handles {
            if let Err(e) = handle.cancel().await {
                log::warn!("MCP shutdown error: {e}");
            }
        }
    }
}

async fn connect_server(
    name: &str,
    cfg: &ServerConfig,
) -> Result<(DynRunningService, Peer<RoleClient>), String> {
    if let Some(command) = &cfg.command {
        connect_stdio(name, command, cfg.args.as_deref(), cfg.env.as_ref()).await
    } else if let Some(url) = &cfg.url {
        connect_http(name, url).await
    } else {
        Err(format!(
            "MCP server '{name}': config must have either 'command' or 'url'"
        ))
    }
}

async fn connect_stdio(
    name: &str,
    command: &str,
    args: Option<&[String]>,
    env: Option<&std::collections::HashMap<String, String>>,
) -> Result<(DynRunningService, Peer<RoleClient>), String> {
    let transport = TokioChildProcess::new(Command::new(command).configure(|cmd| {
        if let Some(args) = args {
            cmd.args(args);
        }
        if let Some(env) = env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }
        cmd.stderr(std::process::Stdio::null());
    }))
    .map_err(|e| format!("MCP server '{name}': failed to spawn process: {e}"))?;

    let service: DynRunningService = ClientInfo {
        meta: None,
        protocol_version: Default::default(),
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: format!("zymi-mcp-{name}"),
            title: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
            website_url: None,
            icons: None,
        },
    }
    .into_dyn()
    .serve(transport)
    .await
    .map_err(|e| format!("MCP server '{name}': failed to connect: {e}"))?;

    let peer = service.peer().clone();
    Ok((service, peer))
}

async fn connect_http(
    name: &str,
    url: &str,
) -> Result<(DynRunningService, Peer<RoleClient>), String> {
    let transport = StreamableHttpClientTransport::from_uri(url);

    let service: DynRunningService = ClientInfo {
        meta: None,
        protocol_version: Default::default(),
        capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: format!("zymi-mcp-{name}"),
            title: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: None,
            website_url: None,
            icons: None,
        },
    }
    .into_dyn()
    .serve(transport)
    .await
    .map_err(|e| format!("MCP server '{name}': failed to connect: {e}"))?;

    let peer = service.peer().clone();
    Ok((service, peer))
}

//! Minimal stdio MCP server: `echo` and `env` tools. Used by the stdio tests
//! and handy for trying a spawner config.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ServiceExt, schemars, tool, tool_router, transport::stdio};

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Text {
    text: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct Var {
    name: String,
}

#[derive(Clone)]
struct Echo;

#[tool_router(server_handler)]
impl Echo {
    #[tool(description = "Echo text back")]
    fn echo(&self, Parameters(Text { text }): Parameters<Text>) -> String {
        format!("stdio echo: {text}")
    }

    #[tool(description = "Read an environment variable of the server process")]
    fn env(&self, Parameters(Var { name }): Parameters<Var>) -> String {
        std::env::var(name).unwrap_or_default()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Echo.serve(stdio()).await?.waiting().await?;
    Ok(())
}

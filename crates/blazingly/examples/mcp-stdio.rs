use blazingly::prelude::*;

#[api_model]
struct EchoInput {
    #[min_length(1)]
    message: String,
}

#[api_model]
struct EchoOutput {
    message: String,
}

#[post("/echo", id = "echo.create", summary = "Echo one message")]
#[mcp::tool(
    name = "echo",
    description = "Echo one message",
    risk = "read",
    confirmation = "never",
    idempotent = true,
    expose_output = "full"
)]
#[allow(clippy::unused_async)]
async fn echo(Json(input): Json<EchoInput>) -> Json<EchoOutput> {
    Json(EchoOutput {
        message: input.message,
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app = ExecutableApp::new(routes![echo])?;
    let mut server = blazingly::mcp::JsonRpcServer::new(&app);
    blazingly::mcp::stdio::serve_stdio(&mut server)?;
    Ok(())
}

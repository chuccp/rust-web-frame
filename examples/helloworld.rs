//! The smallest possible application.
//!
//! ```text
//! cargo run --example helloworld
//! curl http://localhost:19009/
//! ```

use rust_web_frame as wf;
use wf::config::load_auto_config;
use wf::web::handler;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = wf::Builder::new(load_auto_config()?);

    builder.get("/", handler(|_req| async { Ok("hello world") }));
    builder.get(
        "/greet/:name",
        handler(|req: wf::web::Request| async move {
            let name = req.param_or("name", "stranger").to_string();
            Ok(wf::web::Message::with_msg(format!("hello {name}")))
        }),
    );

    builder.build().run().await?;
    Ok(())
}

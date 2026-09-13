use bosn_service::{Client, Service};
use kernal_api::async_engine::RuntimeBuilder;
use std::path::PathBuf;
fn main() {
    let mut args = std::env::args_os();
    let _ = args.next();
    let verb = args.next().unwrap_or_else(|| "status".into());
    let state = PathBuf::from(args.next().unwrap_or_else(|| ".bosn-rs".into()));
    let runtime = match RuntimeBuilder::multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("bosn-rs: runtime initialization: {error}");
            std::process::exit(1);
        }
    };
    let result = runtime.run(async {
        match verb.to_string_lossy().as_ref() {
            "serve" => Service::new(state).serve().await.map(|_| String::new()),
            "ping" => {
                Client::for_state(state)?.ping().await?;
                Ok("pong".into())
            }
            "status" => {
                let s = Client::for_state(state)?.status().await?;
                Ok(format!(
                    "{} schema={} resources={} leases={} sessions={} reconciliation_required={}",
                    s.registry_id,
                    s.schema_version,
                    s.resources,
                    s.leases,
                    s.sessions,
                    s.reconciliation_required
                ))
            }
            "shutdown" => {
                Client::for_state(state)?.shutdown().await?;
                Ok("shutdown".into())
            }
            _ => Err(bosn_service::Error::Protocol(
                "usage: serve|ping|status|shutdown",
            )),
        }
    });
    match result {
        Ok(v) => {
            if !v.is_empty() {
                println!("{v}")
            }
        }
        Err(e) => {
            eprintln!("bosn-rs: {e}");
            std::process::exit(1)
        }
    }
}

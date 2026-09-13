//! Test-only endpoint which invokes the public v4 importer from Python tests.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    let state_dir = arguments
        .next()
        .ok_or("usage: migration-import-test STATE_DIR DESTINATION")?;
    let destination = arguments
        .next()
        .ok_or("usage: migration-import-test STATE_DIR DESTINATION")?;
    if arguments.next().is_some() {
        return Err("usage: migration-import-test STATE_DIR DESTINATION".into());
    }
    let report = bosn_registry::import_python_v4(
        &state_dir,
        std::path::Path::new(&state_dir).join("registry.sqlite3"),
        destination,
    )?;
    match bosn_registry::Registry::open_writer(&report.destination) {
        Err(bosn_registry::Error::ReconciliationRequired) => {}
        Ok(_) => return Err("imported registry unexpectedly accepted a writer".into()),
        Err(error) => return Err(Box::new(error)),
    }
    println!("{}", report.registry_id);
    Ok(())
}

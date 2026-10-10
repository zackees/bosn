//! `bosn gc release-registry`: the one-shot operator decision for objects whose registry
//! predates the machine catalog (#545; AGENTS.md "Automatic retention").

/// `bosn gc release-registry` lists every registry id Docker objects name; with `UUID --yes` it
/// records that registry as abandoned so the machine daemon reclaims its objects under every
/// ordinary gate. Nothing is removed by this command.
pub(crate) fn run_gc_release(mut arguments: impl Iterator<Item = std::ffi::OsString>) {
    let mut registry = None;
    let mut yes = false;
    for argument in arguments.by_ref() {
        match argument.to_string_lossy().as_ref() {
            "--yes" if !yes => yes = true,
            value if registry.is_none() && !value.starts_with('-') => {
                registry = Some(value.to_owned());
            }
            _ => release_usage(),
        }
    }
    let Some(registry) = registry else {
        list_registries();
        return;
    };
    if !yes {
        release_usage();
    }
    match bosn_service::managed_retention::release_registry(&registry) {
        Ok(bosn_service::managed_retention::Release::Released) => println!(
            "released registry {registry}: the machine daemon's next maintenance pass reclaims \
             its idle, unpinned objects past their age gates"
        ),
        Ok(bosn_service::managed_retention::Release::AlreadyAbandoned) => {
            println!("registry {registry} is already abandoned; nothing to do");
        }
        Err(detail) => {
            eprintln!("gc release-registry: refused: {detail}");
            std::process::exit(1);
        }
    }
}

fn list_registries() {
    let (counts, unreadable) = bosn_service::managed_retention::labelled_registries();
    for (registry, objects) in counts {
        println!("{registry}  {objects} object(s)");
    }
    for detail in unreadable {
        eprintln!("gc release-registry: partial: {detail}");
    }
}

fn release_usage() -> ! {
    eprintln!(
        "bosn gc release-registry [UUID --yes]\n\
         \n\
         Without arguments, list the registry ids Docker objects carry. With a UUID, record that\n\
         registry as abandoned: the machine daemon then reclaims its objects under every\n\
         ownership, age, liveness and pin gate. Refused for this machine's registry and for any\n\
         registry whose database still exists."
    );
    std::process::exit(2)
}

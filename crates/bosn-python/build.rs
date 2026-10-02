//! Test-only: the `embedded-python-tests` binary links libpython, and a uv-managed
//! or other non-system CPython keeps it outside the loader's search path. Record
//! the interpreter's LIBDIR as an rpath so the test binary starts without
//! `LD_LIBRARY_PATH`. The shipped abi3 extension never enables this feature, so it
//! gets no rpath.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_FEATURE_EMBEDDED_PYTHON_TESTS").is_some() {
        pyo3_build_config::add_libpython_rpath_link_args();
    }
}

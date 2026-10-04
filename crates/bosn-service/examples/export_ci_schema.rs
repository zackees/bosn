//! Emit the CI contract without writing into a read-only source mount.
fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&bosn_service::ci::schema::document()).unwrap()
    );
}

export CARGO_TERM_COLOR=never
soldr cargo test -p bosn-service --lib --locked ci::lifecycle::tests::shared 2>&1 | grep -E "panicked" -A4 | head -30

export CARGO_TERM_COLOR=never
soldr cargo fmt --all --check 2>&1 | head -20
soldr cargo clippy --workspace --exclude bosn-python --all-targets --locked -- -D warnings 2>&1 | grep -E "(error|warning)(\[|:)" -A14 | grep -v profile.dev | head -100
soldr cargo test -p bosn-service --lib --locked 2>&1 | grep -E "test result|FAILED|panicked|^---- " | head -30

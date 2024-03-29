# just manual: https://github.com/casey/just/#readme

_default:
    @just --list

# Formats the source files
format:
	cargo fmt 

# Runs clippy on the sources 
check:
	cargo clippy --locked -- -D warnings -D clippy::pedantic -D clippy::nursery

# Runs unit tests
test:
	cargo test --locked

# Run load test against the prod server
load_test:
	k6 run ./k6/script.js

# Run the server locally
run:
	cargo run 

# Build and deploy the version on this branch to prod
deploy:
	flyctl deploy --ha=false